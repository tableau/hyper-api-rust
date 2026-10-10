// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Health listener for the daemon.
//!
//! The listener serves a liveness probe and a small command channel on the
//! daemon's [control endpoint](super::control): a Unix domain socket in the
//! state directory (mode `0600`, inside a directory that is owned by the
//! current user and closed to group and others), or a named pipe whose DACL
//! admits only the current user on Windows. Other local users cannot open
//! either, so `STOP`, `STATUS` and `REPORT_HYPERD_ERROR` are the owning user's
//! business only. There is no TCP port.
//!
//! The listener is not the single-instance guard; that is the
//! [`DaemonLock`](super::lock::DaemonLock), which the daemon holds for its
//! whole life. The listener only answers the people who already found the
//! endpoint through `daemon.json`.
//!
//! Protocol (line-based, newline-terminated):
//! - `PING\n` -> `PONG hyperdb-mcp <version>\n` (liveness check; the identifying
//!   token proves it's a hyperdb-mcp daemon, not some other program that
//!   happens to answer on the endpoint)
//! - `HEARTBEAT\n` -> `OK\n` (resets idle timer)
//! - `STOP\n` -> `STOPPING\n` (triggers graceful shutdown)
//! - `STATUS\n` -> JSON line with daemon info (reports the *current* hyperd
//!   endpoint, which can change after a restart).
//! - `REPORT_HYPERD_ERROR\n` -> `OK\n` (sets the restart-requested flag --
//!   the monitor task picks it up on its next tick).
//!
//! Known limits, accepted because only the owning user can reach the channel:
//! the server does not bound the length of a request line or the number of
//! connection threads.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use super::control::{self, CONNECTION_IO_TIMEOUT, ControlListener, ControlStream, HealthEndpoint};
use super::discovery::{DaemonInfo, DaemonRecord};

/// Identifying token included in PONG responses. Used to verify that an
/// endpoint is served by a hyperdb-mcp daemon (not some other program).
pub const PONG_TOKEN: &str = "hyperdb-mcp";

/// Upper bound on how long [`HealthListener::run`] waits for a client before
/// re-checking `should_shutdown` under its own power.
///
/// This is a deliberate correctness floor, not a leftover of the old 5 ms poll.
/// [`DaemonState::request_shutdown`]'s wake connection is what makes shutdown
/// *prompt* (sub-millisecond), but every wake can be lost -- no listener
/// registered yet, a listener sharing a `DaemonState` with another one, a
/// connect that never lands. Since every `request_shutdown` caller in the tree
/// follows it with a `join()`, a lost wake against a purely wake-driven loop is
/// a permanent hang rather than a slow shutdown, and the daemon lock is held
/// until that join returns. One second bounds that unconditionally while still
/// costing only one wakeup per second (see issue #274 for the measurements).
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Construct the PONG response with the identifying token and version.
fn pong_response() -> String {
    format!("PONG {PONG_TOKEN} {}\n", crate::version::MCP_VERSION)
}

/// Handle to the health listener, used to check binding success and manage lifecycle.
#[derive(Debug)]
pub struct HealthListener {
    listener: ControlListener,
    endpoint: HealthEndpoint,
}

/// Shared state between the health listener and the daemon main loop.
///
/// Every field is private and reached through the accessor pair that owns its
/// invariant. `shutdown` in particular *must not* be settable directly: a bare
/// `store(true)` looks identical to `request_shutdown()` through
/// `should_shutdown()`, but skips the wake, so the listener stays in its
/// accept wait for a further `ACCEPT_POLL_INTERVAL` instead of returning
/// at once. The other two are private for the same reason -- one way in, one
/// way out, so the pairing cannot be bypassed by a caller who only sees the
/// field.
#[derive(Debug)]
pub struct DaemonState {
    /// Last time any client sent a heartbeat or query.
    /// Read and written through [`Self::idle_duration`] and [`Self::touch`].
    last_activity: Mutex<Instant>,
    /// Signal to shut down the daemon. Written only by
    /// [`Self::request_shutdown`], read only by [`Self::should_shutdown`].
    shutdown: AtomicBool,
    /// Set by clients reporting that hyperd looks dead from over there;
    /// consumed by the daemon's restart monitor. Written only by
    /// [`Self::request_restart`], read-and-cleared only by
    /// [`Self::consume_restart_request`].
    restart_requested: AtomicBool,
    /// Where the listener this state serves is bound, fixed at construction so
    /// [`Self::request_shutdown`] can connect to it to wake the accept loop.
    /// (A path cannot live in an atomic, so what changes at run time is
    /// [`Self::listener_registered`], not this.)
    endpoint: HealthEndpoint,
    /// Whether a [`HealthListener::run`] loop is currently serving
    /// [`Self::endpoint`] and so can be woken by connecting to it.
    listener_registered: AtomicBool,
}

impl DaemonState {
    /// A fresh state for a daemon whose health listener is (or will be) bound
    /// at `endpoint`.
    pub fn new(endpoint: HealthEndpoint) -> Self {
        Self {
            last_activity: Mutex::new(Instant::now()),
            shutdown: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            endpoint,
            listener_registered: AtomicBool::new(false),
        }
    }

    /// Record activity (resets idle timer).
    ///
    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn touch(&self) {
        *self.last_activity.lock().expect("mutex poisoned") = Instant::now();
    }

    /// Duration since the last activity.
    ///
    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn idle_duration(&self) -> Duration {
        self.last_activity.lock().expect("mutex poisoned").elapsed()
    }

    /// Request shutdown and wake a health listener waiting in its accept.
    ///
    /// Every caller in the tree follows this with a `join()` on the listener
    /// thread (`run_daemon`'s step 7, and the test harnesses), so the wake is
    /// what makes that join return in microseconds rather than after a full
    /// `ACCEPT_POLL_INTERVAL`.
    ///
    /// # Memory ordering
    ///
    /// The `SeqCst` here is load-bearing and must not be relaxed to
    /// `Release`/`Acquire`. This half stores `shutdown` then loads
    /// `listener_registered`; [`HealthListener::run`] stores
    /// `listener_registered` then loads `shutdown`. That is Dekker's
    /// algorithm: each thread writes its own flag and reads the other's.
    /// Release/Acquire orders store->store and load->load, but says nothing
    /// about a store followed by a load of a *different* location, so both
    /// loads may return stale values in the same interleaving -- `run` sees no
    /// shutdown and waits, while this sees no registered listener and connects
    /// to nobody. Only `SeqCst` gives the single total order that rules the
    /// interleaving out.
    ///
    /// Both mainstream targets really do permit the reordering, which is worth
    /// recording because the arm64 half is easy to get wrong. On x86-64 a
    /// `Release` store is a plain `mov` with nothing between it and the
    /// following load, so the store buffer may sink it past that load; a
    /// `SeqCst` store is an `xchg`, which is a full barrier. On arm64 the
    /// store is `stlr` either way -- but LLVM lowers an `Acquire` load to
    /// `ldapr` (RCpc) wherever FEAT_LRCPC is available, Apple Silicon
    /// included, and `ldapr` is specifically *not* ordered against a preceding
    /// `stlr`. Only the `ldar` (RCsc) that `SeqCst` emits carries that
    /// guarantee. Both readings are from the assembly rustc actually emits for
    /// this pairing, not from the reference manuals alone. The cost is one
    /// `xchg` on two cold paths.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake_accept_loop();
    }

    /// Nudge the listening endpoint so a waiting accept returns and the loop
    /// re-reads `should_shutdown`.
    ///
    /// Runs on a detached thread. A Unix socket connect is immediate, but a
    /// Windows pipe connect can wait for a busy instance, and this is called
    /// both from `run_daemon`'s async shutdown path -- where it would stall a
    /// tokio worker -- and from the `STOP` handler, ahead of the `STOPPING`
    /// reply. Neither can afford to wait on it, and nothing needs the result:
    /// shutdown is bounded by the loop's own accept timeout whether or not the
    /// wake ever lands.
    ///
    /// Every failure mode is benign and ignored: no listener registered yet
    /// (the loop's pre-accept `should_shutdown` check catches that), or the
    /// listener already gone (connection refused). The wake makes shutdown
    /// prompt; correctness rests on the flag check the loop performs on every
    /// pass.
    fn wake_accept_loop(&self) {
        // Read on the caller's thread, not the spawned one: this load is the
        // second half of the `SeqCst` pairing documented on
        // `request_shutdown`, and moving it off-thread would break it.
        if !self.listener_registered.load(Ordering::SeqCst) {
            return;
        }
        let endpoint = self.endpoint.clone();
        // `Builder::spawn` rather than `thread::spawn`: a best-effort wake must
        // not panic its caller if the process is out of threads. Losing the
        // wake costs at most one `ACCEPT_POLL_INTERVAL` of shutdown latency.
        let spawned = std::thread::Builder::new()
            .name("health-wake".to_string())
            .spawn(move || ControlListener::wake(&endpoint));
        if let Err(error) = spawned {
            debug!(%error, "could not spawn the health listener wake connection");
        }
    }

    /// Mark the listener as serving [`Self::endpoint`]. Called by
    /// [`HealthListener::run`] before its first `should_shutdown` check.
    ///
    /// `SeqCst` for the reason spelled out on [`Self::request_shutdown`]: this
    /// store is the first half of the Dekker pairing whose second half is the
    /// `shutdown` load in [`HealthListener::run`].
    fn register_listener(&self) {
        self.listener_registered.store(true, Ordering::SeqCst);
    }

    /// Forget the listener once it has stopped, so a later `request_shutdown`
    /// does not connect to whatever has since taken the endpoint.
    fn clear_listener(&self) {
        self.listener_registered.store(false, Ordering::SeqCst);
    }

    /// `SeqCst` to match [`Self::request_shutdown`]'s store -- see the memory
    /// ordering note there.
    pub fn should_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Signal that hyperd appears to have died and a restart is needed.
    pub fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::Release);
    }

    /// Atomically read-and-clear the restart-request flag.
    /// Returns true if a restart was requested since the last call.
    pub fn consume_restart_request(&self) -> bool {
        self.restart_requested.swap(false, Ordering::AcqRel)
    }
}

impl HealthListener {
    /// Bind the control endpoint.
    ///
    /// The caller must hold the [`DaemonLock`](super::lock::DaemonLock): on
    /// Unix a leftover socket file from a dead daemon is removed first, which
    /// is only safe while no other daemon can be starting.
    ///
    /// # Errors
    /// Returns `Err` if something live already serves the endpoint, the path is
    /// too long for a Unix socket, or the bind fails for another reason.
    pub fn bind(endpoint: &HealthEndpoint) -> std::io::Result<Self> {
        Ok(Self {
            listener: ControlListener::bind(endpoint)?,
            endpoint: endpoint.clone(),
        })
    }

    /// The endpoint this listener serves.
    pub fn endpoint(&self) -> &HealthEndpoint {
        &self.endpoint
    }

    /// Run the health listener loop. Spawns per-connection threads until shutdown.
    /// Consumes `self` because this is intended to be called from a dedicated thread.
    ///
    /// `info` is shared (`Arc<Mutex<DaemonInfo>>`) so the listener reports the
    /// *current* hyperd endpoint after a restart -- the monitor task updates the
    /// same Arc once a new hyperd is running.
    ///
    /// The loop does not poll on a timer. It waits for a client for up to
    /// `ACCEPT_POLL_INTERVAL` at a time, so an arriving client is accepted with
    /// no added latency and an idle daemon costs one wakeup per second.
    ///
    /// [`DaemonState::request_shutdown`] makes a throwaway connection to this
    /// listener, which returns the wait immediately; the loop re-checks
    /// `should_shutdown` on every pass and drops the wake connection unread.
    /// The accept timeout is the floor underneath that: it bounds shutdown at
    /// one interval even when the wake never arrives, which matters because
    /// every `request_shutdown` caller joins this thread.
    ///
    /// Dropping the listener when this returns removes the socket file (Unix).
    #[expect(
        clippy::needless_pass_by_value,
        reason = "Arcs are cloned into per-connection threads"
    )]
    pub fn run(mut self, state: Arc<DaemonState>, info: Arc<Mutex<DaemonInfo>>) {
        // Register before the first `should_shutdown` check, so a shutdown
        // racing this setup either finds the listener (and wakes it) or is seen
        // by the check below before we ever wait. The guard clears it again on
        // every exit path, including an unwind out of the loop -- dropping the
        // listener frees the endpoint, and a registration still naming it would
        // send a later `request_shutdown` to whatever took it next.
        let _registration = ListenerRegistration::register(&state);

        loop {
            if state.should_shutdown() {
                break;
            }

            match self.listener.accept_timeout(ACCEPT_POLL_INTERVAL) {
                Ok(Some(stream)) => {
                    // The shutdown wake arrives as an ordinary connection.
                    // Re-check before spending a thread on it; a genuine
                    // client arriving in the same instant is dropped, exactly
                    // as the pre-accept check has always dropped it.
                    if state.should_shutdown() {
                        break;
                    }
                    let state = Arc::clone(&state);
                    let info = Arc::clone(&info);
                    std::thread::spawn(move || {
                        handle_client(stream, &state, &info);
                    });
                }
                Ok(None) => {
                    // Timed out (or a client that gave up): loop round and
                    // re-check `should_shutdown`.
                }
                Err(error) => {
                    warn!(error = %error, "health listener accept error");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
        debug!("health listener shut down");
    }
}

/// Keeps [`DaemonState::listener_registered`] true for exactly as long as the
/// listener loop is running.
///
/// A `Drop` impl rather than a call at the end of [`HealthListener::run`]:
/// the loop can unwind (`std::thread::spawn` panics when the process is out of
/// threads), and an unwind that skipped the clear would leave the flag set for
/// an endpoint the dropped listener had just released.
struct ListenerRegistration<'state> {
    state: &'state DaemonState,
}

impl<'state> ListenerRegistration<'state> {
    fn register(state: &'state DaemonState) -> Self {
        state.register_listener();
        Self { state }
    }
}

impl Drop for ListenerRegistration<'_> {
    fn drop(&mut self) {
        self.state.clear_listener();
    }
}

fn status_json(info: &Mutex<DaemonInfo>) -> String {
    let snapshot = info.lock().expect("DaemonInfo mutex poisoned").clone();
    match DaemonRecord::with_current_identity(&snapshot) {
        Ok(record) => serde_json::to_string(&record).unwrap_or_default(),
        Err(error) => {
            warn!(%error, "could not collect daemon executable identity for STATUS");
            serde_json::to_string(&snapshot).unwrap_or_default()
        }
    }
}

fn handle_client(mut stream: ControlStream, state: &DaemonState, info: &Mutex<DaemonInfo>) {
    if stream.set_io_timeout(CONNECTION_IO_TIMEOUT).is_err() {
        return;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let cmd = line.trim();
                let response = match cmd {
                    "PING" => pong_response(),
                    "HEARTBEAT" => {
                        state.touch();
                        "OK\n".to_string()
                    }
                    "STOP" => {
                        state.request_shutdown();
                        "STOPPING\n".to_string()
                    }
                    "STATUS" => format!("{}\n", status_json(info)),
                    "REPORT_HYPERD_ERROR" => {
                        state.request_restart();
                        "OK\n".to_string()
                    }
                    _ => "ERR unknown command\n".to_string(),
                };
                if reader.get_mut().write_all(response.as_bytes()).is_err() {
                    break;
                }
            }
        }
    }
    // Deliver what was written before closing (a pipe instance needs the
    // flush); a peer that already hung up is not an error.
    let _ = reader.into_inner().finish();
}

/// Send a command to the daemon's health endpoint and return the response.
///
/// Uses generous timeouts (2s connect, 5s read) suitable for `STOP`/`STATUS`
/// where the caller is willing to wait. Use [`send_command_with_timeout`] for
/// best-effort fire-and-forget calls (e.g. heartbeat, error reporting).
///
/// # Errors
/// Returns an error if the connection fails or the response cannot be read.
pub fn send_command(endpoint: &HealthEndpoint, command: &str) -> std::io::Result<String> {
    send_command_with_timeout(
        endpoint,
        command,
        Duration::from_secs(2),
        Duration::from_secs(5),
    )
}

/// Best-effort fire-and-forget: tell the running daemon that hyperd appears to
/// be dead from this client's perspective. Uses short timeouts (200ms each) so
/// the calling tool handler isn't stalled if the daemon itself is slow.
/// Errors are logged at debug level and otherwise ignored.
pub fn report_hyperd_error_to_daemon(endpoint: &HealthEndpoint) {
    let timeout = Duration::from_millis(200);
    match send_command_with_timeout(endpoint, "REPORT_HYPERD_ERROR", timeout, timeout) {
        Ok(response) => {
            debug!(response = %response.trim(), "reported hyperd error to daemon");
        }
        Err(e) => {
            debug!(error = %e, "could not report hyperd error to daemon (best-effort)");
        }
    }
}

/// Send a command with caller-specified connect and I/O timeouts.
///
/// The supplied `read_timeout` also bounds writes so every phase of the
/// request is finite without changing this helper's public signature.
///
/// # Errors
/// Returns an error if the connection fails or the response cannot be read
/// within the supplied timeouts.
pub fn send_command_with_timeout(
    endpoint: &HealthEndpoint,
    command: &str,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> std::io::Result<String> {
    let mut stream = control::connect(endpoint, connect_timeout)?;
    let io_deadline = Instant::now().checked_add(read_timeout).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "health command I/O timeout overflows deadline",
        )
    })?;

    let msg = format!("{command}\n");
    let mut written = 0;
    while written < msg.len() {
        stream.set_io_timeout(remaining_io_time(io_deadline)?)?;
        match stream.write(&msg.as_bytes()[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "health command write returned zero bytes",
                ));
            }
            Ok(count) => {
                written += count;
                remaining_io_time(io_deadline)?;
            }
            Err(error) => return Err(normalize_expired_io_error(error, io_deadline)),
        }
    }

    const MAX_HEALTH_RESPONSE_BYTES: usize = 64 * 1024;
    let mut response = Vec::new();
    loop {
        rearm_io_timeout(&mut stream, remaining_io_time(io_deadline)?)?;
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) if response.is_empty() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "health command connection closed before any response was sent",
                ));
            }
            Ok(0) => break,
            Ok(_) if response.len() == MAX_HEALTH_RESPONSE_BYTES => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "health response exceeds the 64 KiB limit",
                ));
            }
            Ok(_) => {
                response.push(byte[0]);
                remaining_io_time(io_deadline)?;
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(error) => return Err(normalize_expired_io_error(error, io_deadline)),
        }
    }

    String::from_utf8(response).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("health response is not valid UTF-8: {error}"),
        )
    })
}

/// Re-arm the per-operation timeout before a read.
///
/// macOS rejects `SO_RCVTIMEO` with `EINVAL` (`InvalidInput`) on a Unix socket
/// whose peer has already replied and closed. The reply is still readable and
/// the previously armed timeout still bounds the read, so only that case is
/// ignored; every other error is returned.
pub(crate) fn rearm_io_timeout(
    stream: &mut ControlStream,
    timeout: Duration,
) -> std::io::Result<()> {
    match stream.set_io_timeout(timeout) {
        Err(error) if error.kind() != std::io::ErrorKind::InvalidInput => Err(error),
        _ => Ok(()),
    }
}

fn remaining_io_time(io_deadline: Instant) -> std::io::Result<Duration> {
    let remaining = io_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "health command I/O deadline expired",
        ))
    } else {
        Ok(remaining)
    }
}

fn normalize_expired_io_error(error: std::io::Error, io_deadline: Instant) -> std::io::Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) || Instant::now() >= io_deadline
    {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "health command I/O deadline expired",
        )
    } else {
        error
    }
}

/// Send PING and verify the response contains the identifying token. Returns
/// `Some(version)` if the responding daemon is a hyperdb-mcp daemon (the version
/// string is the daemon's `MCP_VERSION`), or `None` if connection fails, the
/// response lacks the expected token, or read times out. An empty version string
/// (`Some(String::new())`) is returned if the PONG prefix matches but no version
/// token is present (graceful degradation for forward/backward compat).
///
/// This is the primitive for liveness checks: merely connecting proves nothing
/// about who answered, the identifying token does.
pub fn ping_identified(
    endpoint: &HealthEndpoint,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Option<String> {
    let response =
        send_command_with_timeout(endpoint, "PING", connect_timeout, read_timeout).ok()?;
    // Validate by exact tokens, not a string prefix: a prefix check on
    // "PONG hyperdb-mcp" would also match a foreign reply like
    // "PONG hyperdb-mcpEVIL 1.0.0". Require the first two whitespace-separated
    // tokens to be exactly "PONG" and the token, so only our daemon passes.
    let mut tokens = response.split_whitespace();
    if tokens.next() != Some("PONG") || tokens.next() != Some(PONG_TOKEN) {
        return None;
    }
    // The 3rd token is the daemon's version; absent ⇒ accept with empty
    // version (future-proofing for a token-only reply).
    Some(tokens.next().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread::JoinHandle;

    use serde_json::{Value, json};

    use crate::diagnostics::ReportedPath;

    use super::*;

    /// A per-test state directory and the endpoint inside it. Kept short so
    /// the socket path stays under the platform's `sun_path` limit.
    fn test_endpoint() -> (tempfile::TempDir, HealthEndpoint) {
        let dir = tempfile::tempdir().expect("create test state dir");
        let endpoint = HealthEndpoint::for_new_daemon(dir.path()).expect("test endpoint");
        (dir, endpoint)
    }

    struct TestPeer<T> {
        _dir: tempfile::TempDir,
        endpoint: HealthEndpoint,
        stop: Option<Sender<()>>,
        handle: Option<JoinHandle<Result<T, String>>>,
    }

    impl<T> TestPeer<T> {
        fn finish(mut self) -> Result<T, String> {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            self.handle
                .take()
                .expect("test peer handle must exist")
                .join()
                .map_err(|payload| format!("test peer panicked: {payload:?}"))?
        }
    }

    impl<T> Drop for TestPeer<T> {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn spawn_test_peer<T, F>(script: F) -> TestPeer<T>
    where
        T: Send + 'static,
        F: FnOnce(ControlStream, Receiver<()>) -> Result<T, String> + Send + 'static,
    {
        let (dir, endpoint) = test_endpoint();
        let mut listener = ControlListener::bind(&endpoint).expect("bind test health peer");
        let (stop_tx, stop_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if stop_rx.try_recv().is_ok() {
                    return Err("test peer stopped before accepting a connection".to_string());
                }
                match listener.accept_timeout(Duration::from_millis(20)) {
                    Ok(Some(stream)) => return script(stream, stop_rx),
                    Ok(None) => {
                        if Instant::now() >= accept_deadline {
                            return Err("test peer timed out waiting for a connection".to_string());
                        }
                    }
                    Err(error) => return Err(format!("test peer accept failed: {error}")),
                }
            }
        });

        TestPeer {
            _dir: dir,
            endpoint,
            stop: Some(stop_tx),
            handle: Some(handle),
        }
    }

    /// Read one request line a byte at a time, so no buffered reader swallows
    /// bytes the script may want afterwards.
    fn read_test_command(stream: &mut ControlStream) -> Result<String, String> {
        stream
            .set_io_timeout(Duration::from_secs(1))
            .map_err(|error| format!("set test peer read timeout: {error}"))?;
        let mut request = Vec::new();
        let mut byte = [0];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    request.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                Err(error) => return Err(format!("read test health command: {error}")),
            }
        }
        String::from_utf8(request).map_err(|error| format!("test command not UTF-8: {error}"))
    }

    fn daemon_info(endpoint: &HealthEndpoint) -> DaemonInfo {
        DaemonInfo {
            pid: 4242,
            hyperd_endpoint: "127.0.0.1:54321".to_string(),
            health_endpoint: endpoint.as_str().to_string(),
            started_at: "2026-08-13T12:34:56Z".to_string(),
            version: "0.7.0".to_string(),
        }
    }

    fn expected_status(info: &DaemonInfo) -> Value {
        let executable = std::env::current_exe().unwrap();
        let executable_path = ReportedPath::from_os_str(executable.as_os_str());

        json!({
            "pid": info.pid,
            "hyperd_endpoint": info.hyperd_endpoint,
            "health_endpoint": info.health_endpoint,
            "started_at": info.started_at,
            "version": info.version,
            "identity": {
                "mcp_version": crate::version::mcp_version_string(),
                "executable_path": executable_path
            }
        })
    }

    fn check_status_json(
        label: &str,
        response: &str,
        expected: &Value,
        failures: &mut Vec<String>,
    ) {
        match serde_json::from_str::<Value>(response.trim()) {
            Ok(actual) => {
                if actual != *expected {
                    failures.push(format!(
                        "{label} was not the exact flat enriched record: {actual}"
                    ));
                }
                if actual.get("info").is_some() {
                    failures.push(format!("{label} nested legacy fields under `info`"));
                }
            }
            Err(error) => failures.push(format!("{label} was not JSON: {error}")),
        }
    }

    #[test]
    fn health_status_returns_flat_enriched_record() {
        let public_run_signature: fn(HealthListener, Arc<DaemonState>, Arc<Mutex<DaemonInfo>>) =
            HealthListener::run;
        std::hint::black_box(public_run_signature);

        let (_dir, endpoint) = test_endpoint();
        let listener = HealthListener::bind(&endpoint).unwrap();
        let state = Arc::new(DaemonState::new(endpoint.clone()));
        let info = Arc::new(Mutex::new(daemon_info(&endpoint)));
        let initial_expected = expected_status(&info.lock().unwrap());
        let mut failures = Vec::new();

        match catch_unwind(AssertUnwindSafe(|| status_json(info.as_ref()))) {
            Ok(response) => check_status_json(
                "private STATUS serializer",
                &response,
                &initial_expected,
                &mut failures,
            ),
            Err(_) => failures.push("private STATUS serializer is not implemented".to_string()),
        }

        let run_state = Arc::clone(&state);
        let run_info = Arc::clone(&info);
        let handle = std::thread::spawn(move || listener.run(run_state, run_info));

        let initial_response = send_command(&endpoint, "STATUS").unwrap();
        check_status_json(
            "initial STATUS response",
            &initial_response,
            &initial_expected,
            &mut failures,
        );

        {
            let mut current = info.lock().unwrap();
            current.hyperd_endpoint = "127.0.0.1:60000".to_string();
        }
        let updated_expected = expected_status(&info.lock().unwrap());
        let updated_response = send_command(&endpoint, "STATUS").unwrap();
        check_status_json(
            "STATUS response after shared DaemonInfo update",
            &updated_response,
            &updated_expected,
            &mut failures,
        );

        let _ = send_command(&endpoint, "STOP");
        handle.join().unwrap();

        assert!(
            failures.is_empty(),
            "health STATUS contract failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn slow_drip_response_honors_absolute_io_deadline() {
        const IO_TIMEOUT: Duration = Duration::from_millis(100);
        const DRIP_INTERVAL: Duration = Duration::from_millis(20);
        const DRIP_BYTES: usize = 40;
        const GENEROUS_COMPLETION_BOUND: Duration = Duration::from_millis(500);

        let peer = spawn_test_peer(|mut stream, stop| {
            let request = read_test_command(&mut stream)?;
            if request != "PING\n" {
                return Err(format!("unexpected health command: {request:?}"));
            }

            for _ in 0..DRIP_BYTES {
                if stop.try_recv().is_ok() {
                    return Ok(false);
                }
                if stream.write_all(b"x").is_err() {
                    return Ok(false);
                }
                std::thread::sleep(DRIP_INTERVAL);
            }
            Ok(true)
        });

        let call = catch_unwind(AssertUnwindSafe(|| {
            let started = Instant::now();
            let result = send_command_with_timeout(
                &peer.endpoint,
                "PING",
                Duration::from_secs(1),
                IO_TIMEOUT,
            );
            (result, started.elapsed())
        }));
        let completed_full_drip = peer
            .finish()
            .expect("slow-drip peer must shut down cleanly");
        let (result, elapsed) = match call {
            Ok(outcome) => outcome,
            Err(payload) => resume_unwind(payload),
        };

        assert_eq!(
            result.as_ref().err().map(std::io::Error::kind),
            Some(std::io::ErrorKind::TimedOut),
            "a peer that makes progress without terminating a line must hit the one I/O deadline; got {result:?} after {elapsed:?}"
        );
        assert!(
            elapsed < GENEROUS_COMPLETION_BOUND,
            "100ms I/O budget was extended to {elapsed:?} by slow-drip progress"
        );
        assert!(
            !completed_full_drip,
            "the client waited for the peer's entire 800ms drip instead of enforcing its absolute deadline"
        );
    }

    #[test]
    fn oversized_newline_free_response_is_rejected() {
        const MAX_HEALTH_RESPONSE_BYTES: usize = 64 * 1024;
        const OVERSIZED_RESPONSE_BYTES: usize = MAX_HEALTH_RESPONSE_BYTES + 1;

        let peer = spawn_test_peer(|mut stream, _stop| {
            let request = read_test_command(&mut stream)?;
            if request != "PING\n" {
                return Err(format!("unexpected health command: {request:?}"));
            }
            stream
                .set_io_timeout(Duration::from_secs(1))
                .map_err(|error| format!("set test peer write timeout: {error}"))?;
            let response = vec![b'x'; OVERSIZED_RESPONSE_BYTES];
            let mut emitted = 0;
            while emitted < response.len() {
                match stream.write(&response[emitted..]) {
                    Ok(0) => {
                        return Err(format!(
                            "oversized health peer wrote zero bytes after {emitted} bytes"
                        ));
                    }
                    Ok(written) => emitted += written,
                    // The client stops reading at the cap and hangs up, so a
                    // broken pipe after the cap is the expected end.
                    Err(_) if emitted > MAX_HEALTH_RESPONSE_BYTES => break,
                    Err(error) => {
                        return Err(format!(
                            "oversized health peer stopped after {emitted} bytes: {error}"
                        ));
                    }
                }
            }
            Ok(emitted)
        });

        let call = catch_unwind(AssertUnwindSafe(|| {
            send_command_with_timeout(
                &peer.endpoint,
                "PING",
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
        }));
        let response_bytes = peer
            .finish()
            .expect("oversized-response peer must shut down cleanly");
        let result = match call {
            Ok(outcome) => outcome,
            Err(payload) => resume_unwind(payload),
        };
        let outcome = match &result {
            Ok(response) => format!("accepted {} bytes", response.len()),
            Err(error) => format!("returned {:?}: {error}", error.kind()),
        };

        assert!(response_bytes > MAX_HEALTH_RESPONSE_BYTES);
        assert_eq!(
            result.as_ref().err().map(std::io::Error::kind),
            Some(std::io::ErrorKind::InvalidData),
            "newline-free health responses beyond the 64 KiB protocol limit must be rejected; {outcome}"
        );
    }

    /// Deterministic form of the test below: re-arming the timeout on a
    /// stream whose peer already replied and closed must succeed, and the
    /// reply must still be readable.
    #[test]
    fn rearming_the_timeout_after_the_peer_closed_keeps_the_reply_readable() {
        let (closed_tx, closed_rx) = mpsc::channel::<()>();
        let peer = spawn_test_peer(move |mut stream, _stop| {
            read_test_command(&mut stream)?;
            stream
                .write_all(b"OK\n")
                .map_err(|error| format!("write reply: {error}"))?;
            drop(stream);
            let _ = closed_tx.send(());
            Ok(())
        });
        let mut client = control::connect(&peer.endpoint, Duration::from_secs(1)).unwrap();
        client.write_all(b"X\n").unwrap();
        // Finishing the peer earlier can stop it before it accepts.
        closed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("peer must reply and close");
        peer.finish().expect("peer must finish");
        std::thread::sleep(Duration::from_millis(100));

        rearm_io_timeout(&mut client, Duration::from_secs(1))
            .expect("a closed peer must not fail the re-arm");
        let mut reply = [0_u8; 3];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"OK\n");
    }

    /// A peer that replies and closes at once must not turn into an error when
    /// the client re-arms its read timeout (macOS rejects that with `EINVAL`).
    #[test]
    fn a_reply_followed_by_an_immediate_close_is_still_returned_promptly() {
        let peer = spawn_test_peer(|mut stream, _stop| {
            let request = read_test_command(&mut stream)?;
            if request != "PING\n" {
                return Err(format!("unexpected health command: {request:?}"));
            }
            stream
                .write_all(b"PONG hyperdb-mcp 9.9.9\n")
                .map_err(|error| format!("write reply: {error}"))?;
            drop(stream);
            Ok(())
        });

        let started = Instant::now();
        let result = send_command_with_timeout(
            &peer.endpoint,
            "PING",
            Duration::from_secs(1),
            Duration::from_secs(5),
        );
        peer.finish().expect("reply-and-close peer must finish");
        assert_eq!(
            result.expect("reply must survive the close"),
            "PONG hyperdb-mcp 9.9.9\n"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A peer that reads the request and then closes without writing anything
    /// must be reported as a failed command, not a successful empty response.
    /// `daemon_stop` in `main.rs` treats `Ok(_)` as "the daemon responded" and
    /// exits 0, so an `Ok("")` here would silently mask a peer that never
    /// answered at all.
    #[test]
    fn peer_close_without_any_response_is_an_error_not_empty_success() {
        let peer = spawn_test_peer(|mut stream, _stop| {
            let request = read_test_command(&mut stream)?;
            if request != "PING\n" {
                return Err(format!("unexpected health command: {request:?}"));
            }
            // Close the connection immediately, writing nothing at all.
            drop(stream);
            Ok(())
        });

        let call = catch_unwind(AssertUnwindSafe(|| {
            send_command_with_timeout(
                &peer.endpoint,
                "PING",
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
        }));
        peer.finish()
            .expect("silent-close peer must shut down cleanly");
        let result = match call {
            Ok(outcome) => outcome,
            Err(payload) => resume_unwind(payload),
        };

        assert_eq!(
            result.as_ref().err().map(std::io::Error::kind),
            Some(std::io::ErrorKind::UnexpectedEof),
            "a peer that closes having sent nothing must be reported as an error, not {result:?}"
        );
    }

    /// Deadline for the two tests that assert the *wake* is doing the work.
    ///
    /// Deliberately far below [`ACCEPT_POLL_INTERVAL`], because the accept
    /// timeout floor would otherwise make them pass with the wake deleted: the
    /// loop would simply notice the flag when its interval expired. Measured
    /// shutdown latency through the wake is well under a millisecond, so half
    /// a second is loose enough for a loaded CI runner and still clear of the
    /// one-second floor it has to distinguish itself from.
    const PROMPT_SHUTDOWN_BOUND: Duration = Duration::from_millis(500);

    /// The promptness half of the design. `run_daemon` (and every test
    /// harness) calls `request_shutdown` and then *joins* the listener thread,
    /// and the wake is what makes that join return immediately instead of
    /// after a full [`ACCEPT_POLL_INTERVAL`].
    ///
    /// Bounded through a channel instead of a bare `join()` on purpose, so a
    /// regression fails with a message rather than stalling the suite.
    #[test]
    fn request_shutdown_wakes_a_waiting_accept_loop() {
        let (_dir, endpoint) = test_endpoint();
        let listener = HealthListener::bind(&endpoint).expect("bind test health listener");
        let state = Arc::new(DaemonState::new(endpoint.clone()));
        let info = Arc::new(Mutex::new(daemon_info(&endpoint)));

        let run_state = Arc::clone(&state);
        let run_info = Arc::clone(&info);
        let (finished_tx, finished_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            listener.run(run_state, run_info);
            let _ = finished_tx.send(());
        });

        // Prove the loop serves a real client with no added latency, then idle
        // long enough that it is certainly back inside its accept wait when
        // shutdown is requested, so the elapsed time below measures the wake
        // and not a coincidental interval boundary.
        let pong = send_command_with_timeout(
            &endpoint,
            "PING",
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .expect("the accept loop must serve a real client while waiting");
        assert!(pong.starts_with("PONG"), "unexpected PING reply: {pong:?}");
        std::thread::sleep(Duration::from_millis(100));

        let requested_at = Instant::now();
        state.request_shutdown();
        let woken = finished_rx.recv_timeout(Duration::from_secs(5));
        let elapsed = requested_at.elapsed();
        assert!(
            woken.is_ok(),
            "request_shutdown did not stop the listener within 5s ({elapsed:?})"
        );
        assert!(
            elapsed < PROMPT_SHUTDOWN_BOUND,
            "request_shutdown took {elapsed:?} to stop the listener, past the \
             {PROMPT_SHUTDOWN_BOUND:?} bound: the wake connection did not land and the \
             loop fell through to its {ACCEPT_POLL_INTERVAL:?} accept floor instead"
        );
        server
            .join()
            .expect("health listener thread must not panic");
    }

    /// The floor underneath the wake, and the reason `run` may never wait
    /// without a timeout.
    ///
    /// Clearing the registration makes `request_shutdown` return without
    /// connecting to anything, the same observable outcome as a shutdown that
    /// races registration or a connect that never lands. Every
    /// `request_shutdown` caller in the tree joins the listener thread, so with
    /// nothing but the wake to rely on this is not a slow shutdown but a
    /// permanent one.
    #[test]
    fn shutdown_is_bounded_when_the_wake_cannot_be_delivered() {
        // Comfortably past `ACCEPT_POLL_INTERVAL`, so the test measures
        // "bounded at all" rather than racing the floor it is verifying.
        const LOST_WAKE_SHUTDOWN_BOUND: Duration = Duration::from_secs(5);

        let (_dir, endpoint) = test_endpoint();
        let listener = HealthListener::bind(&endpoint).expect("bind test health listener");
        let state = Arc::new(DaemonState::new(endpoint.clone()));
        let info = Arc::new(Mutex::new(daemon_info(&endpoint)));

        let run_state = Arc::clone(&state);
        let (finished_tx, finished_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            listener.run(run_state, info);
            let _ = finished_tx.send(());
        });

        // Round-trip a real command first: that proves `run` has reached its
        // loop and registered, so the clear below genuinely un-registers a
        // live listener rather than winning a race against startup.
        let pong = send_command_with_timeout(
            &endpoint,
            "PING",
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .expect("the accept loop must serve a real client before the wake is removed");
        assert!(pong.starts_with("PONG"), "unexpected PING reply: {pong:?}");

        state.clear_listener();

        let requested_at = Instant::now();
        state.request_shutdown();
        let finished = finished_rx.recv_timeout(LOST_WAKE_SHUTDOWN_BOUND);
        let elapsed = requested_at.elapsed();
        assert!(
            finished.is_ok(),
            "the listener did not stop within {LOST_WAKE_SHUTDOWN_BOUND:?} ({elapsed:?}) once \
             the wake could not be delivered; shutdown must stay bounded by the loop's own \
             {ACCEPT_POLL_INTERVAL:?} accept timeout, because run_daemon and every test \
             harness join this thread and an unbounded wait there is a permanent hang"
        );
        server
            .join()
            .expect("health listener thread must not panic");
    }

    /// The ordering hazard the pre-accept flag check covers: a shutdown
    /// requested before `run` has registered sends no wake at all, so the loop
    /// must notice the flag on its own rather than waiting out an interval.
    #[test]
    fn run_returns_promptly_when_shutdown_precedes_it() {
        let (_dir, endpoint) = test_endpoint();
        let listener = HealthListener::bind(&endpoint).expect("bind test health listener");
        let state = Arc::new(DaemonState::new(endpoint.clone()));
        let info = Arc::new(Mutex::new(daemon_info(&endpoint)));

        state.request_shutdown();

        let run_state = Arc::clone(&state);
        let (finished_tx, finished_rx) = mpsc::channel();
        let started_at = Instant::now();
        let server = std::thread::spawn(move || {
            listener.run(run_state, info);
            let _ = finished_tx.send(());
        });

        let finished = finished_rx.recv_timeout(Duration::from_secs(5));
        let elapsed = started_at.elapsed();
        assert!(
            finished.is_ok() && elapsed < PROMPT_SHUTDOWN_BOUND,
            "run() must observe a pre-existing shutdown flag before waiting on the socket; \
             it returned after {elapsed:?}, which means it waited out an accept interval \
             ({ACCEPT_POLL_INTERVAL:?}) first"
        );
        server
            .join()
            .expect("health listener thread must not panic");
    }

    /// Dropping the listener releases the endpoint, so a second listener can
    /// bind it. (Single-instance is the lock's job, not the socket's.)
    #[test]
    fn dropping_the_listener_frees_the_endpoint() {
        let (_dir, endpoint) = test_endpoint();
        let first = HealthListener::bind(&endpoint).expect("first bind");
        drop(first);
        HealthListener::bind(&endpoint).expect("rebind after drop");
    }
}
