// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! TLS for PG-wire TCP connections.
//!
//! A [`TlsConfig`] picks a [`TlsMode`] (the libpq `sslmode` names) and,
//! optionally, a root certificate, a client certificate for mutual TLS, and a
//! server-name override. It is set with
//! [`Config::with_tls`](super::Config::with_tls).
//!
//! When a mode other than [`TlsMode::Disable`] is set, the client sends an
//! `SSLRequest` on the fresh TCP connection before the startup message, reads
//! the server's one-byte answer straight off the socket, and runs a rustls
//! handshake on `'S'`. A cancel request for a TLS session is sent over TLS
//! too, and never falls back to plaintext. TLS applies to TCP only; gRPC
//! picks TLS through its `https://` endpoint scheme.
//!
//! # Certificate files
//!
//! | Item | Format |
//! |------|--------|
//! | Root certificate(s) | PEM, one or more `-----BEGIN CERTIFICATE-----` blocks |
//! | Client certificate | PEM, leaf first, optionally followed by intermediates |
//! | Client key | PEM: PKCS#8 (`BEGIN PRIVATE KEY`), PKCS#1 (`BEGIN RSA PRIVATE KEY`) or SEC1 (`BEGIN EC PRIVATE KEY`) |
//!
//! # Example
//!
//! ```
//! use hyperdb_api_core::client::Config;
//! use hyperdb_api_core::client::tls::{TlsConfig, TlsMode};
//!
//! // Verify the server's chain against our CA and its name against the host.
//! let config = Config::new()
//!     .with_host("hyper.example.com")
//!     .with_tls(TlsConfig::new(TlsMode::VerifyFull).root_cert("ca.pem"));
//! assert_eq!(config.tls().mode(), TlsMode::VerifyFull);
//!
//! // A bare mode converts into a `TlsConfig`.
//! let config = Config::new().with_tls(TlsMode::Require);
//! assert_eq!(config.tls().mode(), TlsMode::Require);
//!
//! // The libpq names parse.
//! assert_eq!("verify-ca".parse::<TlsMode>(), Ok(TlsMode::VerifyCa));
//! ```

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv6Addr, Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme,
    StreamOwned,
};
use tracing::warn;

use super::error::{Error, Result};
use crate::protocol::message::frontend;

/// How a TCP connection uses TLS, named after libpq's `sslmode`.
///
/// | Mode | Server without TLS | Server certificate check |
/// |---|---|---|
/// | `Disable` (default) | plaintext; no `SSLRequest` is sent | — |
/// | `Prefer` | plaintext | none, or chain-only if a root certificate is set |
/// | `Require` | error | none, or chain-only if a root certificate is set |
/// | `VerifyCa` | error | chain against the root certificate (required) |
/// | `VerifyFull` | error | chain and host name, against the root certificate or else the bundled Mozilla roots |
///
/// A configured root certificate is the **only** trust anchor. "None" still
/// checks the handshake signatures; it skips only the chain and the name.
///
/// Where libpq and sqlx/tokio-postgres differ, this follows sqlx:
///
/// - The default is `Disable`, not libpq's `prefer`: `hyperd` is usually a
///   local engine with TLS off.
/// - libpq's `allow` (plaintext first, TLS only if the server insists) is not
///   offered; use `Prefer`.
/// - `Prefer` falls back to plaintext only when the server answers that it
///   has no TLS. A failed handshake or certificate check is an error, never a
///   downgrade.
/// - `VerifyFull` without a root certificate trusts the bundled
///   `webpki-roots` (Mozilla) set, not the OS store.
/// - Default certificate files such as `~/.postgresql/root.crt` are never
///   read.
/// - Over a Unix domain socket or a named pipe, `Prefer` connects in
///   plaintext and the other enabled modes are an error rather than ignored.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TlsMode {
    /// Never use TLS (the default).
    #[default]
    Disable,
    /// Use TLS when the server offers it, plaintext otherwise.
    Prefer,
    /// Require TLS; check the certificate chain only if a root certificate
    /// is set.
    Require,
    /// Require TLS and a certificate chain to the configured root
    /// certificate. The host name is not checked.
    VerifyCa,
    /// Require TLS, a trusted certificate chain, and a certificate that
    /// names the host.
    VerifyFull,
}

impl TlsMode {
    /// The libpq `sslmode` spelling: `"disable"`, `"prefer"`, `"require"`,
    /// `"verify-ca"` or `"verify-full"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Disable => "disable",
            TlsMode::Prefer => "prefer",
            TlsMode::Require => "require",
            TlsMode::VerifyCa => "verify-ca",
            TlsMode::VerifyFull => "verify-full",
        }
    }
}

impl fmt::Display for TlsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TlsMode {
    type Err = ParseTlsModeError;

    /// Parses the libpq `sslmode` spelling (case-sensitive, as in libpq).
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "disable" => Ok(TlsMode::Disable),
            "prefer" => Ok(TlsMode::Prefer),
            "require" => Ok(TlsMode::Require),
            "verify-ca" => Ok(TlsMode::VerifyCa),
            "verify-full" => Ok(TlsMode::VerifyFull),
            _ => Err(ParseTlsModeError {
                input: s.to_string(),
            }),
        }
    }
}

/// The error returned when a string is not a [`TlsMode`] name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTlsModeError {
    input: String,
}

impl fmt::Display for ParseTlsModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid TLS mode `{}`; expected one of: disable, prefer, require, verify-ca, verify-full",
            self.input
        )?;
        if self.input == "allow" {
            f.write_str(" (libpq's `allow` is not supported; use `prefer`)")?;
        }
        Ok(())
    }
}

impl std::error::Error for ParseTlsModeError {}

/// TLS settings for a TCP connection.
///
/// Built from a [`TlsMode`] with optional files; see the mode table on
/// [`TlsMode`] for what each mode checks. The files are read when the
/// connection is opened, and an unreadable or empty file is a configuration
/// error then.
///
/// ```
/// use hyperdb_api_core::client::tls::{TlsConfig, TlsMode};
///
/// let tls = TlsConfig::new(TlsMode::VerifyFull)
///     .root_cert("ca.pem")
///     .client_cert("client.pem", "client.key")
///     .server_name("hyper.internal");
/// assert_eq!(tls.mode(), TlsMode::VerifyFull);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[must_use = "TlsConfig uses a consuming builder pattern; use the returned value"]
pub struct TlsConfig {
    mode: TlsMode,
    root_cert: Option<PathBuf>,
    client_cert: Option<(PathBuf, PathBuf)>,
    server_name: Option<String>,
}

impl TlsConfig {
    /// A configuration for `mode` with no files and no server-name override.
    pub fn new(mode: TlsMode) -> Self {
        TlsConfig {
            mode,
            ..TlsConfig::default()
        }
    }

    /// Trust only the certificates in this PEM file (libpq `sslrootcert`).
    ///
    /// Required for [`TlsMode::VerifyCa`]. With [`TlsMode::Prefer`] or
    /// [`TlsMode::Require`] it turns on chain verification.
    pub fn root_cert(mut self, pem_path: impl AsRef<Path>) -> Self {
        self.root_cert = Some(pem_path.as_ref().to_path_buf());
        self
    }

    /// Offer this client certificate and key for mutual TLS (libpq `sslcert`
    /// and `sslkey`). Both are PEM files.
    pub fn client_cert(mut self, cert_path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Self {
        self.client_cert = Some((
            cert_path.as_ref().to_path_buf(),
            key_path.as_ref().to_path_buf(),
        ));
        self
    }

    /// Use this name instead of the connection host for SNI and for the
    /// [`TlsMode::VerifyFull`] host-name check.
    pub fn server_name(mut self, name: impl Into<String>) -> Self {
        self.server_name = Some(name.into());
        self
    }

    /// The configured mode.
    #[must_use]
    pub fn mode(&self) -> TlsMode {
        self.mode
    }
}

impl From<TlsMode> for TlsConfig {
    fn from(mode: TlsMode) -> Self {
        TlsConfig::new(mode)
    }
}

/// A ready-to-use client TLS setup: the built rustls configuration and the
/// name to present and verify. Built once per connection and kept for the
/// connection's cancel requests, so the PEM files are not re-read.
#[derive(Debug)]
pub(crate) struct TlsConnector {
    config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
}

impl TlsConnector {
    /// Builds the connector for `host`, or `Ok(None)` for
    /// [`TlsMode::Disable`].
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when a file cannot be read or holds no usable PEM
    /// item, when [`TlsMode::VerifyCa`] has no root certificate, when the
    /// client certificate and key do not form a pair, or when the server
    /// name is not a valid DNS name or IP address.
    pub(crate) fn build(tls: &TlsConfig, host: &str) -> Result<Option<Arc<Self>>> {
        if tls.mode == TlsMode::Disable {
            return Ok(None);
        }

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let algorithms = provider.signature_verification_algorithms;
        let roots = tls.root_cert.as_deref().map(load_roots).transpose()?;

        let verifier: Arc<dyn ServerCertVerifier> = match (tls.mode, roots) {
            (TlsMode::Disable, _) => return Ok(None),
            (TlsMode::Prefer | TlsMode::Require, None) => Arc::new(NoVerify { algorithms }),
            (TlsMode::Prefer | TlsMode::Require | TlsMode::VerifyCa, Some(roots)) => {
                Arc::new(ChainOnly {
                    roots: Arc::new(roots),
                    algorithms,
                })
            }
            (TlsMode::VerifyCa, None) => {
                return Err(Error::config(
                    "TlsMode::VerifyCa needs a root certificate; set TlsConfig::root_cert",
                ));
            }
            (TlsMode::VerifyFull, roots) => {
                let roots = roots.unwrap_or_else(|| {
                    let mut store = RootCertStore::empty();
                    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                    store
                });
                WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                    .build()
                    .map_err(|e| Error::config(format!("cannot build TLS verifier: {e}")))?
            }
        };

        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::config(format!("TLS protocol configuration: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier);
        let mut config = match &tls.client_cert {
            Some((cert_path, key_path)) => builder
                .with_client_auth_cert(
                    load_certs(cert_path, "client certificate")?,
                    load_key(key_path)?,
                )
                .map_err(|e| Error::config(format!("invalid client certificate or key: {e}")))?,
            None => builder.with_no_client_auth(),
        };
        // hyperd requests client certificates (`verify_peer`) without setting
        // an OpenSSL session ID context, so it aborts every resumed handshake
        // with an `internal_error` alert. The cancel request reuses this
        // connector and would resume. libpq never resumes either: it builds a
        // fresh `SSL_CTX` per connection.
        config.resumption = rustls::client::Resumption::disabled();

        let name = tls.server_name.as_deref().unwrap_or_else(|| {
            // An IPv6 host arrives as `[::1]`; the brackets are URL syntax.
            let host = host
                .strip_prefix('[')
                .and_then(|inner| inner.strip_suffix(']'))
                .unwrap_or(host);
            // A link-local literal can carry a zone (`fe80::1%1`). The zone
            // picks the local interface, not the server, and an IP SAN cannot
            // hold one, so the name is the bare address.
            match host.split_once('%') {
                Some((address, _zone)) if address.parse::<Ipv6Addr>().is_ok() => address,
                _ => host,
            }
        });
        let server_name = ServerName::try_from(name)
            .map_err(|e| Error::config(format!("invalid TLS server name `{name}`: {e}")))?
            .to_owned();

        Ok(Some(Arc::new(TlsConnector {
            config: Arc::new(config),
            server_name,
        })))
    }
}

fn load_certs(path: &Path, what: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect::<std::result::Result<Vec<_>, _>>)
        .map_err(|e| Error::config(format!("cannot read {what} `{}`: {e}", path.display())))?;
    if certs.is_empty() {
        return Err(Error::config(format!(
            "{what} `{}` contains no PEM certificate",
            path.display()
        )));
    }
    Ok(certs)
}

fn load_roots(path: &Path) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for cert in load_certs(path, "root certificate")? {
        roots.add(cert).map_err(|e| {
            Error::config(format!(
                "invalid root certificate in `{}`: {e}",
                path.display()
            ))
        })?;
    }
    Ok(roots)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    // `from_pem_file` reports a file with no key section as `NoItemsFound`.
    PrivateKeyDer::from_pem_file(path)
        .map_err(|e| Error::config(format!("cannot read client key `{}`: {e}", path.display())))
}

/// [`TlsMode::Prefer`] / [`TlsMode::Require`] without a root certificate:
/// accept any certificate, but still verify the handshake signatures, so the
/// peer has to hold the key for the certificate it presents.
#[derive(Debug)]
struct NoVerify {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// [`TlsMode::VerifyCa`], and `Prefer` / `Require` with a root certificate:
/// the chain must lead to `roots`; the name is not checked. This is the chain
/// step `WebPkiServerVerifier` runs before its name check, called directly.
#[derive(Debug)]
struct ChainOnly {
    roots: Arc<RootCertStore>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.algorithms.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// What to do when the server answers the `SSLRequest` with `'N'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// Continue in plaintext on the same socket ([`TlsMode::Prefer`]).
    AllowPlaintext,
    /// Fail with [`Error::Tls`]. Every enabled mode but `Prefer`, and every
    /// cancel request.
    Deny,
}

impl Fallback {
    /// The fallback a connection in `mode` uses.
    pub(crate) fn for_mode(mode: TlsMode) -> Self {
        if mode == TlsMode::Prefer {
            Fallback::AllowPlaintext
        } else {
            Fallback::Deny
        }
    }
}

/// Checks `tls` for a Unix-domain-socket or named-pipe connection, which has
/// no TLS: [`TlsMode::Prefer`] connects in plaintext, as libpq does over a
/// Unix socket, and every mode that requires TLS is rejected rather than
/// silently downgraded.
pub(crate) fn reject_on_ipc(tls: &TlsConfig) -> Result<()> {
    match tls.mode() {
        TlsMode::Disable | TlsMode::Prefer => Ok(()),
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => {
            Err(Error::feature_not_supported(format!(
                "TLS mode {} is only available over TCP; use a host:port endpoint",
                tls.mode()
            )))
        }
    }
}

/// The outcome of a negotiation: the socket unchanged (after `'N'` with
/// [`Fallback::AllowPlaintext`]), or the TLS stream.
pub(crate) enum Negotiated<Plain, Tls> {
    Plain(Plain),
    Tls(Tls),
}

/// How long a cancel request may spend on TLS negotiation and on sending.
/// A cancel runs from `Drop` too, so it must not hang on a silent server.
pub(crate) const CANCEL_TLS_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a TLS cancel waits for the server to close after `close_notify`.
const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

const NO_TLS_MESSAGE: &str = "server does not support TLS (answered 'N' to SSLRequest)";

const EARLY_CLOSE_MESSAGE: &str = "server closed the connection during TLS negotiation";

fn ssl_request_bytes() -> BytesMut {
    let mut buf = BytesMut::with_capacity(8);
    frontend::ssl_request(&mut buf);
    buf
}

fn unexpected_reply(byte: u8) -> Error {
    Error::protocol(format!(
        "unexpected response to SSLRequest: {:?} (expected 'S' or 'N')",
        char::from(byte)
    ))
}

fn timed_out(timeout: Option<Duration>) -> Error {
    match timeout {
        Some(limit) => Error::timeout(format!("TLS negotiation timed out after {limit:?}")),
        None => Error::timeout("TLS negotiation timed out"),
    }
}

/// Classifies an I/O error raised while negotiating: a socket timeout is
/// [`Error::Timeout`], an early close is reported as such, and everything
/// else goes through [`Error::from_io`] (so rustls failures are `Tls`).
fn negotiation_error(err: io::Error, timeout: Option<Duration>) -> Error {
    match err.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => timed_out(timeout),
        io::ErrorKind::UnexpectedEof
            if err
                .get_ref()
                .is_none_or(|inner| !inner.is::<rustls::Error>()) =>
        {
            Error::io(EARLY_CLOSE_MESSAGE)
        }
        _ => Error::from_io(err),
    }
}

/// Sets the socket timeouts to what is left until `deadline`, so the limit
/// holds for the negotiation as a whole: a peer that answers each read just
/// in time cannot stretch it. No deadline leaves the socket blocking.
fn arm(tcp: &TcpStream, deadline: Option<Instant>, timeout: Option<Duration>) -> Result<()> {
    let Some(deadline) = deadline else {
        return Ok(());
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    // A zero timeout is `InvalidInput` to the socket; it means time is up.
    if remaining.is_zero() {
        return Err(timed_out(timeout));
    }
    tcp.set_read_timeout(Some(remaining))
        .and_then(|()| tcp.set_write_timeout(Some(remaining)))
        .map_err(Error::from_io)
}

/// Negotiates TLS on a freshly connected socket, before the startup message.
///
/// `timeout` bounds the whole negotiation; the socket timeouts it uses are
/// cleared again before returning. The answer byte is read straight off the
/// socket, so nothing the server sends after it can be mistaken for
/// plaintext.
pub(crate) fn negotiate_sync(
    mut tcp: TcpStream,
    connector: &TlsConnector,
    fallback: Fallback,
    timeout: Option<Duration>,
) -> Result<Negotiated<TcpStream, StreamOwned<ClientConnection, TcpStream>>> {
    // An unrepresentable deadline (a timeout of centuries) is no deadline.
    let deadline = timeout.and_then(|limit| Instant::now().checked_add(limit));

    let mut reply = [0u8; 1];
    arm(&tcp, deadline, timeout)?;
    tcp.write_all(&ssl_request_bytes())
        .map_err(|e| negotiation_error(e, timeout))?;
    arm(&tcp, deadline, timeout)?;
    tcp.read_exact(&mut reply)
        .map_err(|e| negotiation_error(e, timeout))?;

    match reply[0] {
        b'S' => {}
        b'N' => {
            return match fallback {
                Fallback::AllowPlaintext => {
                    clear_timeouts(&tcp)?;
                    Ok(Negotiated::Plain(tcp))
                }
                Fallback::Deny => Err(Error::tls(NO_TLS_MESSAGE)),
            };
        }
        other => return Err(unexpected_reply(other)),
    }

    let mut conn =
        ClientConnection::new(Arc::clone(&connector.config), connector.server_name.clone())
            .map_err(|e| Error::tls(e.to_string()))?;
    // Driven one `read_tls` / `write_tls` at a time rather than with
    // `complete_io`, which keeps reading for as long as bytes arrive: the
    // deadline has to be re-armed before every socket operation. Looping
    // until nothing is left to write also sends the client's Finished (and
    // certificate, for mutual TLS) now rather than with the first
    // application write.
    while conn.is_handshaking() || conn.wants_write() {
        arm(&tcp, deadline, timeout)?;
        if conn.wants_write() {
            match conn.write_tls(&mut tcp) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(negotiation_error(e, timeout)),
            }
            continue;
        }
        match conn.read_tls(&mut tcp) {
            Ok(0) => return Err(Error::io(EARLY_CLOSE_MESSAGE)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(negotiation_error(e, timeout)),
        }
        if let Err(e) = conn.process_new_packets() {
            // Last-gasp attempt to send the alert describing the failure.
            let _ = conn.write_tls(&mut tcp);
            return Err(Error::tls(e.to_string()));
        }
    }

    clear_timeouts(&tcp)?;
    Ok(Negotiated::Tls(StreamOwned::new(conn, tcp)))
}

fn clear_timeouts(tcp: &TcpStream) -> Result<()> {
    tcp.set_read_timeout(None).map_err(Error::from_io)?;
    tcp.set_write_timeout(None).map_err(Error::from_io)
}

/// Async [`negotiate_sync`]: the whole negotiation runs under `timeout`.
pub(crate) async fn negotiate_async(
    mut tcp: tokio::net::TcpStream,
    connector: &TlsConnector,
    fallback: Fallback,
    timeout: Option<Duration>,
) -> Result<Negotiated<tokio::net::TcpStream, tokio_rustls::client::TlsStream<tokio::net::TcpStream>>>
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let negotiation = async move {
        let mut reply = [0u8; 1];
        let request = async {
            tcp.write_all(&ssl_request_bytes()).await?;
            tcp.flush().await?;
            tcp.read_exact(&mut reply).await
        };
        request.await.map_err(|e| negotiation_error(e, timeout))?;

        match reply[0] {
            b'S' => {}
            b'N' => {
                return match fallback {
                    Fallback::AllowPlaintext => Ok(Negotiated::Plain(tcp)),
                    Fallback::Deny => Err(Error::tls(NO_TLS_MESSAGE)),
                };
            }
            other => return Err(unexpected_reply(other)),
        }

        let stream = tokio_rustls::TlsConnector::from(Arc::clone(&connector.config))
            .connect(connector.server_name.clone(), tcp)
            .await
            .map_err(|e| negotiation_error(e, timeout))?;
        Ok(Negotiated::Tls(stream))
    };

    match timeout {
        Some(limit) => tokio::time::timeout(limit, negotiation)
            .await
            .map_err(|_| timed_out(timeout))?,
        None => negotiation.await,
    }
}

fn cancel_connect_error(addr: &str, err: &io::Error) -> Error {
    warn!(
        target: "hyperdb_api",
        addr = %addr,
        error = %err,
        "query-cancel-connect-failed"
    );
    Error::connection(format!(
        "failed to connect for cancel request to {addr}: {err}"
    ))
}

fn cancel_send_error(err: io::Error) -> Error {
    warn!(
        target: "hyperdb_api",
        error = %err,
        "query-cancel-send-failed"
    );
    Error::from_io(err)
}

/// Sends a `CancelRequest` for (`process_id`, `secret_key`) to the TCP
/// address `addr` (`host:port`), over TLS when `tls` is set.
///
/// Blocking and runtime-free, because it also runs from `Drop`. Over TLS the
/// negotiation never falls back to plaintext and is bounded as a whole by
/// [`CANCEL_TLS_TIMEOUT`], as is each write of the request. After the
/// request the client sends `close_notify`, shuts down its write half and
/// drains the server's remaining bytes (session tickets) for up to a second,
/// so closing the socket with unread data cannot turn into a reset that
/// drops the request. Like the plaintext cancel, the TCP connect itself has
/// no timeout.
pub(crate) fn send_cancel_sync(
    addr: &str,
    process_id: i32,
    secret_key: i32,
    tls: Option<&TlsConnector>,
) -> Result<()> {
    let mut tcp = TcpStream::connect(addr).map_err(|e| cancel_connect_error(addr, &e))?;
    // Cancel is a 16-byte fire-and-forget — disable Nagle so the request
    // hits the wire without waiting on a coalesce timer.
    tcp.set_nodelay(true).ok();

    let mut buf = BytesMut::with_capacity(16);
    frontend::cancel_request(process_id, secret_key, &mut buf);

    let Some(connector) = tls else {
        tcp.write_all(&buf).map_err(cancel_send_error)?;
        return tcp.flush().map_err(Error::from_io);
    };

    let mut stream = match negotiate_sync(tcp, connector, Fallback::Deny, Some(CANCEL_TLS_TIMEOUT))?
    {
        Negotiated::Tls(stream) => stream,
        Negotiated::Plain(_) => return Err(Error::tls(NO_TLS_MESSAGE)),
    };
    stream
        .sock
        .set_write_timeout(Some(CANCEL_TLS_TIMEOUT))
        .map_err(Error::from_io)?;
    stream.write_all(&buf).map_err(cancel_send_error)?;
    stream.flush().map_err(cancel_send_error)?;

    // The request is out; the rest is a best-effort orderly close.
    stream.conn.send_close_notify();
    while stream.conn.wants_write() {
        if stream.conn.write_tls(&mut stream.sock).is_err() {
            break;
        }
    }
    stream.sock.shutdown(Shutdown::Write).ok();
    drain_sync(&mut stream.sock);
    Ok(())
}

fn drain_sync(sock: &mut TcpStream) {
    let deadline = Instant::now() + CANCEL_DRAIN_TIMEOUT;
    let mut sink = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || sock.set_read_timeout(Some(remaining)).is_err() {
            return;
        }
        match sock.read(&mut sink) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// Async [`send_cancel_sync`], with the same TLS behaviour.
pub(crate) async fn send_cancel_async(
    addr: &str,
    process_id: i32,
    secret_key: i32,
    tls: Option<&TlsConnector>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| cancel_connect_error(addr, &e))?;
    // Cancel is a 16-byte fire-and-forget — disable Nagle so the request
    // hits the wire without waiting on a coalesce timer.
    tcp.set_nodelay(true).ok();

    let mut buf = BytesMut::with_capacity(16);
    frontend::cancel_request(process_id, secret_key, &mut buf);

    let Some(connector) = tls else {
        tcp.write_all(&buf).await.map_err(cancel_send_error)?;
        return tcp.flush().await.map_err(Error::from_io);
    };

    let mut stream =
        match negotiate_async(tcp, connector, Fallback::Deny, Some(CANCEL_TLS_TIMEOUT)).await? {
            Negotiated::Tls(stream) => stream,
            Negotiated::Plain(_) => return Err(Error::tls(NO_TLS_MESSAGE)),
        };
    let send = async {
        stream.write_all(&buf).await?;
        stream.flush().await
    };
    tokio::time::timeout(CANCEL_TLS_TIMEOUT, send)
        .await
        .map_err(|_| Error::timeout("timed out sending the cancel request"))?
        .map_err(cancel_send_error)?;

    // The request is out; the rest is a best-effort orderly close.
    // `shutdown` sends `close_notify`, then shuts down the write half.
    let close = async {
        stream.shutdown().await?;
        let mut sink = [0u8; 4096];
        while stream.read(&mut sink).await? > 0 {}
        Ok::<(), io::Error>(())
    };
    let _ = tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, close).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! A fake server on a `std::net::TcpListener` thread checks that the
    //! client sends an `SSLRequest`, answers `'S'`, `'N'` or `'E'`, runs a
    //! rustls server on `'S'`, and reports what it received. This proves
    //! what goes on the wire, which `hyperd` cannot: it accepts a plaintext
    //! cancel even from a TLS session.

    use super::*;
    use std::net::{SocketAddr, TcpListener};
    use std::sync::mpsc;
    use std::thread;

    use rustls::ServerConnection;
    use rustls::server::{ServerConfig, WebPkiClientVerifier};
    use tempfile::NamedTempFile;

    struct TestCa {
        issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
        cert_pem: String,
    }

    struct TestCert {
        cert_pem: String,
        key_pem: String,
    }

    fn generate_ca(name: &str) -> TestCa {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        TestCa {
            cert_pem: cert.pem(),
            issuer: rcgen::Issuer::new(params, key_pair),
        }
    }

    /// A leaf signed by `ca`. `sans` may mix DNS names and IP literals.
    fn generate_leaf(ca: &TestCa, sans: &[&str]) -> TestCert {
        let sans = sans.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let params = rcgen::CertificateParams::new(sans).unwrap();
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key_pair, &ca.issuer).unwrap();
        TestCert {
            cert_pem: cert.pem(),
            key_pem: key_pair.serialize_pem(),
        }
    }

    fn pem_file(pem: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(pem.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    /// Server certificates for the common cases: a CA and a leaf for
    /// `localhost` and `127.0.0.1`.
    struct Pki {
        ca: TestCa,
        server: TestCert,
        ca_file: NamedTempFile,
    }

    fn pki() -> Pki {
        let ca = generate_ca("Test CA");
        let server = generate_leaf(&ca, &["localhost", "127.0.0.1"]);
        let ca_file = pem_file(&ca.cert_pem);
        Pki {
            ca,
            server,
            ca_file,
        }
    }

    fn server_config(server: &TestCert, client_ca_pem: Option<&str>) -> Arc<ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs = CertificateDer::pem_slice_iter(server.cert_pem.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(server.key_pem.as_bytes()).unwrap();
        let builder = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap();
        let config = match client_ca_pem {
            Some(ca_pem) => {
                let mut roots = RootCertStore::empty();
                for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
                    roots.add(cert.unwrap()).unwrap();
                }
                let verifier =
                    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                        .build()
                        .unwrap();
                builder
                    .with_client_cert_verifier(verifier)
                    .with_single_cert(certs, key)
            }
            None => builder.with_no_client_auth().with_single_cert(certs, key),
        };
        Arc::new(config.unwrap())
    }

    /// What the fake server saw after the `SSLRequest`.
    #[derive(Debug)]
    #[expect(
        dead_code,
        reason = "the diagnostic payloads are read only through Debug in assertion messages"
    )]
    enum Observed {
        /// The first 8 bytes were not an `SSLRequest`.
        NotSslRequest(Vec<u8>),
        /// After `'N'`: every plaintext byte until EOF.
        Plain(Vec<u8>),
        /// After `'S'`: up to 16 decrypted bytes, and whether the client
        /// then closed with `close_notify` (rather than a bare EOF).
        Tls { data: Vec<u8>, close_notify: bool },
        /// After `'S'`: the server-side handshake failed.
        HandshakeFailed(String),
        /// After `'E'`: nothing further is read.
        Refused,
    }

    const IO_TIMEOUT: Duration = Duration::from_secs(5);

    /// Accepts one connection, answers its `SSLRequest` with `reply`, and
    /// reports what followed. `tls` is required when `reply` is `b'S'`.
    fn spawn_server(
        reply: u8,
        tls: Option<Arc<ServerConfig>>,
    ) -> (SocketAddr, mpsc::Receiver<Observed>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            sock.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
            let mut request = [0u8; 8];
            if sock.read_exact(&mut request).is_err() || request != ssl_request_bytes()[..] {
                tx.send(Observed::NotSslRequest(request.to_vec())).ok();
                return;
            }
            sock.write_all(&[reply]).unwrap();
            let observed = match reply {
                b'S' => serve_tls(sock, tls.expect("TLS config for 'S'")),
                b'N' => {
                    let mut data = Vec::new();
                    sock.read_to_end(&mut data).ok();
                    Observed::Plain(data)
                }
                _ => Observed::Refused,
            };
            tx.send(observed).ok();
        });
        (addr, rx)
    }

    fn serve_tls(mut sock: TcpStream, config: Arc<ServerConfig>) -> Observed {
        let mut conn = ServerConnection::new(config).unwrap();
        while conn.is_handshaking() {
            if let Err(e) = conn.complete_io(&mut sock) {
                return Observed::HandshakeFailed(e.to_string());
            }
        }
        let mut stream = StreamOwned::new(conn, sock);
        let mut data = vec![0u8; 16];
        let mut filled = 0;
        while filled < data.len() {
            match stream.read(&mut data[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => return Observed::HandshakeFailed(e.to_string()),
            }
        }
        data.truncate(filled);
        // rustls reports a `close_notify` as a clean EOF and a bare TCP close
        // as `UnexpectedEof`.
        let mut rest = Vec::new();
        let close_notify = stream.read_to_end(&mut rest).is_ok();
        Observed::Tls { data, close_notify }
    }

    fn observed(rx: &mpsc::Receiver<Observed>) -> Observed {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("fake server reported nothing")
    }

    const PAYLOAD: &[u8; 16] = b"sixteen-byte-msg";

    fn connector(tls: &TlsConfig, host: &str) -> Arc<TlsConnector> {
        TlsConnector::build(tls, host)
            .expect("connector builds")
            .expect("mode is enabled")
    }

    /// Negotiates against `addr`, writes [`PAYLOAD`] over whatever was
    /// negotiated, and closes. Returns whether TLS was negotiated.
    fn negotiate_and_send(addr: SocketAddr, tls: &TlsConfig, host: &str) -> Result<bool> {
        let tcp = TcpStream::connect(addr).unwrap();
        let negotiated = negotiate_sync(
            tcp,
            &connector(tls, host),
            Fallback::for_mode(tls.mode()),
            Some(IO_TIMEOUT),
        )?;
        match negotiated {
            Negotiated::Plain(mut tcp) => {
                tcp.write_all(PAYLOAD).unwrap();
                Ok(false)
            }
            Negotiated::Tls(mut stream) => {
                stream.write_all(PAYLOAD).map_err(Error::from_io)?;
                stream.flush().map_err(Error::from_io)?;
                Ok(true)
            }
        }
    }

    /// Runs one handshake against a fresh fake TLS server and returns the
    /// client outcome together with what the server saw.
    fn handshake(server: &TestCert, tls: &TlsConfig, host: &str) -> (Result<bool>, Observed) {
        let (addr, rx) = spawn_server(b'S', Some(server_config(server, None)));
        let outcome = negotiate_and_send(addr, tls, host);
        (outcome, observed(&rx))
    }

    fn assert_payload_received(observed: &Observed) {
        assert!(
            matches!(observed, Observed::Tls { data, .. } if data == PAYLOAD),
            "server saw {observed:?}"
        );
    }

    // ---- Modes and verification ------------------------------------------

    #[test]
    fn require_accepts_unknown_ca_without_root_cert() {
        let pki = pki();
        let (outcome, seen) = handshake(&pki.server, &TlsMode::Require.into(), "localhost");
        assert!(outcome.expect("Require without root cert accepts any cert"));
        assert_payload_received(&seen);
    }

    #[test]
    fn require_with_wrong_root_cert_checks_the_chain() {
        let pki = pki();
        let other = pem_file(&generate_ca("Other CA").cert_pem);
        let tls = TlsConfig::new(TlsMode::Require).root_cert(other.path());
        let (outcome, _) = handshake(&pki.server, &tls, "localhost");
        assert!(matches!(outcome, Err(Error::Tls(_))), "got {outcome:?}");
    }

    #[test]
    fn verify_ca_accepts_right_ca() {
        let pki = pki();
        let tls = TlsConfig::new(TlsMode::VerifyCa).root_cert(pki.ca_file.path());
        let (outcome, seen) = handshake(&pki.server, &tls, "localhost");
        assert!(outcome.unwrap());
        assert_payload_received(&seen);
    }

    #[test]
    fn verify_ca_rejects_wrong_ca() {
        let pki = pki();
        let other = pem_file(&generate_ca("Other CA").cert_pem);
        let tls = TlsConfig::new(TlsMode::VerifyCa).root_cert(other.path());
        let (outcome, _) = handshake(&pki.server, &tls, "localhost");
        let err = outcome.unwrap_err();
        assert!(matches!(err, Error::Tls(_)), "got {err:?}");
        assert!(err.to_string().contains("UnknownIssuer"), "got {err}");
    }

    #[test]
    fn verify_ca_ignores_the_host_name() {
        let pki = pki();
        let tls = TlsConfig::new(TlsMode::VerifyCa).root_cert(pki.ca_file.path());
        let (outcome, seen) = handshake(&pki.server, &tls, "wrong.example");
        assert!(outcome.unwrap());
        assert_payload_received(&seen);
    }

    #[test]
    fn verify_ca_without_root_cert_is_config_error() {
        let err = TlsConnector::build(&TlsMode::VerifyCa.into(), "localhost").unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
    }

    #[test]
    fn verify_full_accepts_matching_host() {
        let pki = pki();
        let tls = TlsConfig::new(TlsMode::VerifyFull).root_cert(pki.ca_file.path());
        let (outcome, seen) = handshake(&pki.server, &tls, "localhost");
        assert!(outcome.unwrap());
        assert_payload_received(&seen);
    }

    #[test]
    fn verify_full_rejects_wrong_host() {
        let pki = pki();
        let tls = TlsConfig::new(TlsMode::VerifyFull).root_cert(pki.ca_file.path());
        let (outcome, _) = handshake(&pki.server, &tls, "wrong.example");
        let err = outcome.unwrap_err();
        assert!(matches!(err, Error::Tls(_)), "got {err:?}");
        assert!(err.to_string().contains("not valid for name"), "got {err}");
    }

    #[test]
    fn verify_full_server_name_override_replaces_host() {
        let pki = pki();
        let tls = TlsConfig::new(TlsMode::VerifyFull)
            .root_cert(pki.ca_file.path())
            .server_name("localhost");
        let (outcome, seen) = handshake(&pki.server, &tls, "wrong.example");
        assert!(outcome.unwrap());
        assert_payload_received(&seen);

        // And the override is what gets checked.
        let tls = TlsConfig::new(TlsMode::VerifyFull)
            .root_cert(pki.ca_file.path())
            .server_name("wrong.example");
        let (outcome, _) = handshake(&pki.server, &tls, "localhost");
        assert!(matches!(outcome, Err(Error::Tls(_))), "got {outcome:?}");
    }

    #[test]
    fn verify_full_checks_ip_host_against_ip_san() {
        let pki = pki();
        let tls = TlsConfig::new(TlsMode::VerifyFull).root_cert(pki.ca_file.path());
        let (outcome, seen) = handshake(&pki.server, &tls, "127.0.0.1");
        assert!(outcome.unwrap());
        assert_payload_received(&seen);

        // An IP the certificate does not name fails.
        let (outcome, _) = handshake(&pki.server, &tls, "127.0.0.2");
        assert!(matches!(outcome, Err(Error::Tls(_))), "got {outcome:?}");
    }

    #[test]
    fn verify_full_without_root_cert_uses_bundled_roots() {
        // The test CA is not a Mozilla root, so the chain fails; this proves
        // the bundled store is the one in use rather than "no verification".
        let pki = pki();
        let (outcome, _) = handshake(&pki.server, &TlsMode::VerifyFull.into(), "localhost");
        let err = outcome.unwrap_err();
        assert!(err.to_string().contains("UnknownIssuer"), "got {err}");
    }

    #[test]
    fn ipv6_host_brackets_are_stripped() {
        let tls = TlsConfig::from(TlsMode::Require);
        let built = connector(&tls, "[::1]");
        assert!(
            matches!(built.server_name, ServerName::IpAddress(_)),
            "got {:?}",
            built.server_name
        );
        let built = connector(&tls, "localhost");
        assert!(matches!(built.server_name, ServerName::DnsName(_)));
        let err = TlsConnector::build(&tls, "not a host").unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
    }

    #[test]
    fn ipv6_zone_is_not_part_of_the_server_name() {
        let tls = TlsConfig::from(TlsMode::Require);
        for host in ["[fe80::1%1]", "[fe80::1%en0]", "fe80::1%1"] {
            let built = connector(&tls, host);
            let expected = ServerName::from("fe80::1".parse::<std::net::IpAddr>().unwrap());
            assert_eq!(built.server_name, expected, "{host}");
        }
        // Only an IPv6 address loses its `%` suffix.
        let err = TlsConnector::build(&tls, "host%1").unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
    }

    #[test]
    fn verify_full_checks_zoned_ipv6_host_against_ip_san() {
        let ca = generate_ca("Test CA");
        let server = generate_leaf(&ca, &["fe80::1"]);
        let ca_file = pem_file(&ca.cert_pem);
        let tls = TlsConfig::new(TlsMode::VerifyFull).root_cert(ca_file.path());
        let (outcome, seen) = handshake(&server, &tls, "[fe80::1%1]");
        assert!(outcome.unwrap());
        assert_payload_received(&seen);

        let (outcome, _) = handshake(&server, &tls, "[fe80::2%1]");
        assert!(matches!(outcome, Err(Error::Tls(_))), "got {outcome:?}");
    }

    #[test]
    fn disable_builds_no_connector() {
        assert!(
            TlsConnector::build(&TlsConfig::default(), "localhost")
                .unwrap()
                .is_none()
        );
    }

    // ---- Server answers and fallback --------------------------------------

    #[test]
    fn prefer_does_not_downgrade_after_failed_verification() {
        let pki = pki();
        let other = pem_file(&generate_ca("Other CA").cert_pem);
        let tls = TlsConfig::new(TlsMode::Prefer).root_cert(other.path());
        let (outcome, seen) = handshake(&pki.server, &tls, "localhost");
        assert!(matches!(outcome, Err(Error::Tls(_))), "got {outcome:?}");
        assert!(
            matches!(seen, Observed::HandshakeFailed(_)),
            "server saw {seen:?}"
        );
    }

    #[test]
    fn prefer_falls_back_to_plaintext_on_n() {
        let (addr, rx) = spawn_server(b'N', None);
        let negotiated_tls = negotiate_and_send(addr, &TlsMode::Prefer.into(), "localhost");
        assert!(!negotiated_tls.unwrap(), "Prefer + 'N' must be plaintext");
        assert!(
            matches!(observed(&rx), Observed::Plain(ref data) if data == PAYLOAD),
            "the payload follows in plaintext on the same socket"
        );
    }

    #[test]
    fn require_fails_on_n() {
        let (addr, rx) = spawn_server(b'N', None);
        let outcome = negotiate_and_send(addr, &TlsMode::Require.into(), "localhost");
        assert!(matches!(outcome, Err(Error::Tls(_))), "got {outcome:?}");
        assert!(
            matches!(observed(&rx), Observed::Plain(ref data) if data.is_empty()),
            "nothing may be sent in plaintext"
        );
    }

    #[test]
    fn unexpected_reply_is_protocol_error() {
        let (addr, rx) = spawn_server(b'E', None);
        let outcome = negotiate_and_send(addr, &TlsMode::Require.into(), "localhost");
        assert!(
            matches!(outcome, Err(Error::Protocol(_))),
            "got {outcome:?}"
        );
        assert!(matches!(observed(&rx), Observed::Refused));
    }

    #[test]
    fn fallback_for_mode() {
        assert_eq!(
            Fallback::for_mode(TlsMode::Prefer),
            Fallback::AllowPlaintext
        );
        for mode in [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull] {
            assert_eq!(Fallback::for_mode(mode), Fallback::Deny, "{mode}");
        }
    }

    #[test]
    fn negotiation_times_out_on_a_silent_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let holder = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(3));
            drop(sock);
        });
        let started = Instant::now();
        let outcome = negotiate_sync(
            TcpStream::connect(addr).unwrap(),
            &connector(&TlsMode::Require.into(), "localhost"),
            Fallback::Deny,
            Some(Duration::from_millis(500)),
        );
        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, Err(Error::Timeout(_))),
            "got {:?}",
            outcome.err()
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
        holder.join().unwrap();
    }

    /// The timeout bounds the negotiation as a whole: a server that keeps
    /// every single read inside the limit still cannot stretch it.
    #[test]
    fn negotiation_deadline_holds_against_a_dripping_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let dripper = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut request = [0u8; 8];
            sock.read_exact(&mut request).unwrap();
            // 'S', then the header of a handshake record announcing 16 KiB,
            // whose body arrives one byte at a time: rustls keeps waiting
            // for the rest of the record.
            sock.write_all(&[b'S', 0x16, 0x03, 0x03, 0x40, 0x00])
                .unwrap();
            for _ in 0..20 {
                thread::sleep(Duration::from_millis(150));
                if sock.write_all(&[0]).is_err() {
                    break;
                }
            }
        });
        let started = Instant::now();
        let outcome = negotiate_sync(
            TcpStream::connect(addr).unwrap(),
            &connector(&TlsMode::Require.into(), "localhost"),
            Fallback::Deny,
            Some(Duration::from_millis(600)),
        );
        let elapsed = started.elapsed();
        let err = outcome.err().expect("negotiation must time out");
        assert!(matches!(err, Error::Timeout(_)), "got {err:?}");
        assert_eq!(err.to_string(), "TLS negotiation timed out after 600ms");
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
        dripper.join().unwrap();
    }

    #[test]
    fn zero_timeout_is_a_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let outcome = negotiate_sync(
            TcpStream::connect(listener.local_addr().unwrap()).unwrap(),
            &connector(&TlsMode::Require.into(), "localhost"),
            Fallback::Deny,
            Some(Duration::ZERO),
        );
        assert!(
            matches!(outcome, Err(Error::Timeout(_))),
            "got {:?}",
            outcome.err()
        );
    }

    // ---- Mutual TLS -------------------------------------------------------

    #[test]
    fn mtls_client_cert_from_the_ca_is_accepted() {
        let pki = pki();
        let client = generate_leaf(&pki.ca, &["client"]);
        let (cert, key) = (pem_file(&client.cert_pem), pem_file(&client.key_pem));
        let (addr, rx) = spawn_server(
            b'S',
            Some(server_config(&pki.server, Some(&pki.ca.cert_pem))),
        );
        let tls = TlsConfig::new(TlsMode::VerifyFull)
            .root_cert(pki.ca_file.path())
            .client_cert(cert.path(), key.path());
        assert!(negotiate_and_send(addr, &tls, "localhost").unwrap());
        assert_payload_received(&observed(&rx));
    }

    #[test]
    fn mtls_missing_client_cert_fails_on_first_read_as_tls() {
        let pki = pki();
        let (addr, rx) = spawn_server(
            b'S',
            Some(server_config(&pki.server, Some(&pki.ca.cert_pem))),
        );
        let tls = TlsConfig::new(TlsMode::VerifyFull).root_cert(pki.ca_file.path());
        // Under TLS 1.3 the client's handshake completes before the server
        // has looked at the (absent) certificate; the rejection arrives as an
        // alert on the first read.
        let Negotiated::Tls(mut stream) = negotiate_sync(
            TcpStream::connect(addr).unwrap(),
            &connector(&tls, "localhost"),
            Fallback::Deny,
            Some(IO_TIMEOUT),
        )
        .expect("the client side of the handshake completes") else {
            panic!("expected TLS");
        };
        let err = stream
            .read(&mut [0u8; 1])
            .map_err(Error::from_io)
            .unwrap_err();
        assert!(matches!(err, Error::Tls(_)), "got {err:?}");
        assert!(
            matches!(observed(&rx), Observed::HandshakeFailed(_)),
            "server must reject the connection"
        );
    }

    // ---- Cancel senders ---------------------------------------------------

    fn assert_cancel_received(observed: &Observed) {
        let Observed::Tls { data, close_notify } = observed else {
            panic!("server saw {observed:?}");
        };
        let mut expected = BytesMut::new();
        frontend::cancel_request(4242, -7, &mut expected);
        assert_eq!(data[..], expected[..], "decrypted CancelRequest");
        assert!(*close_notify, "cancel must end with close_notify");
    }

    #[test]
    fn cancel_sync_sends_the_request_over_tls() {
        let pki = pki();
        let (addr, rx) = spawn_server(b'S', Some(server_config(&pki.server, None)));
        let tls = connector(&TlsMode::Require.into(), "localhost");
        send_cancel_sync(&addr.to_string(), 4242, -7, Some(&tls)).unwrap();
        assert_cancel_received(&observed(&rx));
    }

    #[test]
    fn cancel_sync_never_falls_back_to_plaintext() {
        let (addr, rx) = spawn_server(b'N', None);
        // Even a connector built for `Prefer` must not fall back.
        let tls = connector(&TlsMode::Prefer.into(), "localhost");
        let err = send_cancel_sync(&addr.to_string(), 4242, -7, Some(&tls)).unwrap_err();
        assert!(matches!(err, Error::Tls(_)), "got {err:?}");
        assert!(
            matches!(observed(&rx), Observed::Plain(ref data) if data.is_empty()),
            "no plaintext CancelRequest may follow"
        );
    }

    #[test]
    fn cancel_sync_without_tls_sends_plaintext() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reader = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            sock.read_to_end(&mut data).unwrap();
            data
        });
        send_cancel_sync(&addr.to_string(), 4242, -7, None).unwrap();
        let mut expected = BytesMut::new();
        frontend::cancel_request(4242, -7, &mut expected);
        assert_eq!(reader.join().unwrap(), expected[..]);
    }

    #[tokio::test]
    async fn cancel_async_sends_the_request_over_tls() {
        let pki = pki();
        let (addr, rx) = spawn_server(b'S', Some(server_config(&pki.server, None)));
        let tls = connector(&TlsMode::Require.into(), "localhost");
        send_cancel_async(&addr.to_string(), 4242, -7, Some(&tls))
            .await
            .unwrap();
        assert_cancel_received(&observed(&rx));
    }

    #[tokio::test]
    async fn cancel_async_never_falls_back_to_plaintext() {
        let (addr, rx) = spawn_server(b'N', None);
        let tls = connector(&TlsMode::Prefer.into(), "localhost");
        let err = send_cancel_async(&addr.to_string(), 4242, -7, Some(&tls))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tls(_)), "got {err:?}");
        assert!(
            matches!(observed(&rx), Observed::Plain(ref data) if data.is_empty()),
            "no plaintext CancelRequest may follow"
        );
    }

    // ---- Async negotiation ------------------------------------------------

    async fn negotiate_async_and_send(
        addr: SocketAddr,
        tls: &TlsConfig,
        host: &str,
    ) -> Result<bool> {
        use tokio::io::AsyncWriteExt;

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let negotiated = negotiate_async(
            tcp,
            &connector(tls, host),
            Fallback::for_mode(tls.mode()),
            Some(IO_TIMEOUT),
        )
        .await?;
        match negotiated {
            Negotiated::Plain(mut tcp) => {
                tcp.write_all(PAYLOAD).await.unwrap();
                Ok(false)
            }
            Negotiated::Tls(mut stream) => {
                stream.write_all(PAYLOAD).await.map_err(Error::from_io)?;
                stream.flush().await.map_err(Error::from_io)?;
                Ok(true)
            }
        }
    }

    #[tokio::test]
    async fn negotiate_async_require() {
        let pki = pki();
        let (addr, rx) = spawn_server(b'S', Some(server_config(&pki.server, None)));
        let tls = TlsConfig::new(TlsMode::Require);
        assert!(
            negotiate_async_and_send(addr, &tls, "localhost")
                .await
                .unwrap()
        );
        assert_payload_received(&observed(&rx));
    }

    #[tokio::test]
    async fn negotiate_async_verify_full() {
        let pki = pki();
        let tls = TlsConfig::new(TlsMode::VerifyFull).root_cert(pki.ca_file.path());

        let (addr, rx) = spawn_server(b'S', Some(server_config(&pki.server, None)));
        assert!(
            negotiate_async_and_send(addr, &tls, "localhost")
                .await
                .unwrap()
        );
        assert_payload_received(&observed(&rx));

        let (addr, _rx) = spawn_server(b'S', Some(server_config(&pki.server, None)));
        let outcome = negotiate_async_and_send(addr, &tls, "wrong.example").await;
        assert!(matches!(outcome, Err(Error::Tls(_))), "got {outcome:?}");
    }

    // ---- Configuration ----------------------------------------------------

    #[test]
    fn tls_mode_parses_and_displays_libpq_names() {
        for mode in [
            TlsMode::Disable,
            TlsMode::Prefer,
            TlsMode::Require,
            TlsMode::VerifyCa,
            TlsMode::VerifyFull,
        ] {
            assert_eq!(mode.to_string().parse::<TlsMode>(), Ok(mode));
        }
        assert_eq!(TlsMode::VerifyCa.to_string(), "verify-ca");
        assert_eq!(TlsMode::default(), TlsMode::Disable);

        let err = "verify_full".parse::<TlsMode>().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("`verify_full`"), "got {message}");
        for name in ["disable", "prefer", "require", "verify-ca", "verify-full"] {
            assert!(message.contains(name), "{name} missing from {message}");
        }
        assert!(!message.contains("allow"), "got {message}");

        let message = "allow".parse::<TlsMode>().unwrap_err().to_string();
        assert!(message.contains("use `prefer`"), "got {message}");
    }

    #[test]
    fn tls_config_builder_and_from_mode() {
        let tls = TlsConfig::new(TlsMode::VerifyFull)
            .root_cert("/ca.pem")
            .client_cert("/c.pem", "/c.key")
            .server_name("db.internal");
        assert_eq!(tls.mode(), TlsMode::VerifyFull);
        assert_eq!(tls.root_cert.as_deref(), Some(Path::new("/ca.pem")));
        assert_eq!(
            tls.client_cert,
            Some((PathBuf::from("/c.pem"), PathBuf::from("/c.key")))
        );
        assert_eq!(tls.server_name.as_deref(), Some("db.internal"));
        assert_eq!(
            TlsConfig::from(TlsMode::Require),
            TlsConfig::new(TlsMode::Require)
        );
        assert_eq!(TlsConfig::default().mode(), TlsMode::Disable);
    }

    #[test]
    fn missing_or_empty_pem_files_are_config_errors() {
        let missing = TlsConfig::new(TlsMode::VerifyCa).root_cert("/nonexistent/ca.pem");
        let err = TlsConnector::build(&missing, "localhost").unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
        assert!(err.to_string().contains("/nonexistent/ca.pem"), "got {err}");

        let empty = pem_file("");
        let err = TlsConnector::build(
            &TlsConfig::new(TlsMode::VerifyCa).root_cert(empty.path()),
            "localhost",
        )
        .unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
        assert!(err.to_string().contains("no PEM certificate"), "got {err}");

        let pki = pki();
        let cert = pem_file(&pki.server.cert_pem);
        let err = TlsConnector::build(
            &TlsConfig::new(TlsMode::Require).client_cert(cert.path(), empty.path()),
            "localhost",
        )
        .unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
    }
}
