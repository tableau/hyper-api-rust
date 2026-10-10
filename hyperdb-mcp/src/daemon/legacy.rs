// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Retiring a pre-1.0 daemon (rc.5 and earlier). Marked for removal after 1.x.
//!
//! Those daemons publish `health_port` in `daemon.json`, serve the health
//! protocol on `127.0.0.1:<port>` over TCP, and hold no daemon lock. A new
//! client that simply spawned would get the lock and then fail its `hyperd`
//! socket pre-flight against the old engine, leaving every client in local mode
//! until the old daemon exits, which by default is never. So, under the daemon
//! lock, the client asks the old daemon to stop and waits for its port to go
//! quiet.
//!
//! The old daemon is only sent `STOP` after it has proved to be the one the
//! record names: an identified `PING`, then a `STATUS` whose pid equals the
//! record's. Anything else is a stale record pointing at a port somebody else
//! may now hold, and nothing further is sent to it.
//!
//! Only a refused connection means "nothing is listening". A timeout, reset or
//! partial reply from an open port is "unknown": the record is kept, nothing
//! more is sent and the client runs in local mode without spawning. A prompt,
//! complete reply that is not an identified `PONG` is a foreign service, so the
//! record is stale.
//!
//! Caveats, accepted: a legacy daemon that ignores `STOP` costs up to
//! [`STOP_WAIT`](super::spawn::STOP_WAIT) and another `STOP` on every client start until it exits; the
//! old daemon removes its own record during its shutdown, which can race this
//! module's removal (harmless, both only delete the same legacy file); each
//! command uses its own connection; a probe costs about a second at worst; and
//! a state directory that is not trusted never reaches this code (the client
//! uses local mode without retiring anything).
//!
//! This module uses only `std` (plus serde/tracing), so it compiles unchanged on
//! every platform.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::{info, warn};

use super::discovery;
use super::health::PONG_TOKEN;

/// Budget for one probe (connect, or the whole request/reply exchange).
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Polling interval while waiting for the port to go quiet.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Longest reply read from a legacy port. A `STATUS` line is far smaller; the
/// cap keeps a foreign listener that streams bytes from growing the buffer.
const MAX_REPLY_BYTES: usize = 16 * 1024;

/// The two fields of a legacy `daemon.json` this module acts on. Unknown
/// fields are ignored, so an `identity` block written by a late rc is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(super) struct LegacyRecord {
    pub(super) pid: u32,
    pub(super) health_port: u16,
}

impl LegacyRecord {
    /// Parse a legacy-shaped record. A record that carries `health_endpoint`
    /// is the current shape and is never a legacy record, whatever else it has.
    pub(super) fn parse(bytes: &[u8]) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        if value.get("health_endpoint").is_some() {
            return None;
        }
        let record: Self = serde_json::from_value(value).ok()?;
        (record.health_port != 0).then_some(record)
    }
}

/// How a legacy record resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retirement {
    /// The port refuses connections, or what answers there is definitely not
    /// the recorded daemon: the record is stale and nothing was sent beyond
    /// `PING`/`STATUS`.
    Stale,
    /// The recorded daemon was told to stop and its port went quiet.
    Retired,
    /// The port is open but did not answer in a way that identifies the
    /// recorded daemon. The record is kept and nothing more was sent.
    Unresponsive,
    /// The recorded daemon was told to stop and still answered after the wait.
    StillRunning,
}

/// What one probe of a legacy port found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    /// The connection was refused: nothing listens on the port.
    Refused,
    /// A complete (newline-terminated) reply line.
    Line(String),
    /// Anything else: connect or read timeout, reset, EOF or a reply with no
    /// line end, an oversized reply. Something may well be listening, so this
    /// is never treated as "dead".
    Unknown,
}

/// Retire a legacy daemon named by `dir`'s record, if there is one.
///
/// The caller must hold the daemon lock for `dir`. On `Ok` no legacy daemon is
/// running and any legacy record has been removed, so the caller may spawn. The
/// record is removed only here, under the lock, so it can never later delete a
/// new daemon's record.
///
/// # Errors
/// Returns [`io::ErrorKind::TimedOut`] if the port is open but did not identify
/// the recorded daemon, or the daemon is still answering after `stop_wait`.
/// The record is kept and the caller must not spawn.
pub(super) fn retire_under_lock(dir: &Path, stop_wait: Duration) -> io::Result<()> {
    let Some(record) = discovery::read_legacy_record(dir) else {
        return Ok(());
    };
    match retire(record, stop_wait) {
        Retirement::Stale => {
            info!(
                port = record.health_port,
                "removing a stale pre-1.0 daemon record"
            );
        }
        Retirement::Retired => {
            info!(pid = record.pid, "retired a pre-1.0 daemon");
        }
        Retirement::Unresponsive => {
            warn!(
                port = record.health_port,
                "pre-1.0 daemon port is open but did not identify the recorded daemon"
            );
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the pre-1.0 daemon record names port {}, which is open but did not \
                     identify the recorded daemon (pid {}); nothing was sent to it",
                    record.health_port, record.pid
                ),
            ));
        }
        Retirement::StillRunning => {
            warn!(
                pid = record.pid,
                port = record.health_port,
                "pre-1.0 daemon did not stop within the timeout"
            );
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "a pre-1.0 daemon (pid {}) is still running on port {} and did not stop \
                     within {} seconds; stop it manually or wait for it to exit",
                    record.pid,
                    record.health_port,
                    stop_wait.as_secs()
                ),
            ));
        }
    }
    discovery::remove_legacy_record(dir, record);
    Ok(())
}

/// Only a refused connection means "nothing there". A timeout, reset or
/// garbled reply from an open port is "unknown" and keeps the record; a
/// prompt, complete reply that is not an identified `PONG` is a foreign
/// service, so the record is stale.
fn retire(record: LegacyRecord, stop_wait: Duration) -> Retirement {
    let port = record.health_port;
    match classify_ping(port) {
        Ping::Identified => {}
        Ping::Dead | Ping::Foreign => return Retirement::Stale,
        Ping::Unknown => return Retirement::Unresponsive,
    }
    // Separate connections for each command: the old daemon may close after
    // every reply.
    match status_pid(port) {
        StatusPid::Pid(pid) if pid == record.pid => {}
        StatusPid::Pid(_) | StatusPid::Refused => return Retirement::Stale,
        StatusPid::Unknown => return Retirement::Unresponsive,
    }

    // Best effort: an error here means the daemon may already be going away,
    // which the wait below settles.
    let _ = exchange(port, "STOP");

    // The old daemon removes its own record before it releases the port, so a
    // quiet port also means its record is gone (or ours to remove).
    let deadline = Instant::now() + stop_wait;
    loop {
        match classify_ping(port) {
            Ping::Dead | Ping::Foreign => return Retirement::Retired,
            Ping::Identified | Ping::Unknown => {}
        }
        if Instant::now() >= deadline {
            return Retirement::StillRunning;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ping {
    /// A complete `PONG hyperdb-mcp ...` line.
    Identified,
    /// A complete reply line that is anything else.
    Foreign,
    /// The connection was refused.
    Dead,
    /// No usable answer from a port that may be open.
    Unknown,
}

fn classify_ping(port: u16) -> Ping {
    match exchange(port, "PING") {
        Probe::Refused => Ping::Dead,
        Probe::Unknown => Ping::Unknown,
        Probe::Line(line) => {
            let mut tokens = line.split_whitespace();
            if tokens.next() == Some("PONG") && tokens.next() == Some(PONG_TOKEN) {
                Ping::Identified
            } else {
                Ping::Foreign
            }
        }
    }
}

enum StatusPid {
    Pid(u32),
    Refused,
    Unknown,
}

/// The pid a legacy daemon reports for itself in `STATUS`.
fn status_pid(port: u16) -> StatusPid {
    match exchange(port, "STATUS") {
        Probe::Refused => StatusPid::Refused,
        Probe::Unknown => StatusPid::Unknown,
        Probe::Line(line) => serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()
            .and_then(|value| u32::try_from(value.get("pid")?.as_u64()?).ok())
            .map_or(StatusPid::Unknown, StatusPid::Pid),
    }
}

/// Send one command on a fresh connection to `127.0.0.1:port` and return the
/// first reply line. Connecting and the exchange each take at most
/// [`PROBE_TIMEOUT`], so one probe costs about a second at worst.
fn exchange(port: u16, command: &str) -> Probe {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = match TcpStream::connect_timeout(&address, PROBE_TIMEOUT) {
        Ok(stream) => stream,
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => return Probe::Refused,
        Err(_) => return Probe::Unknown,
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    if stream.set_write_timeout(Some(PROBE_TIMEOUT)).is_err()
        || stream.write_all(command.as_bytes()).is_err()
        || stream.write_all(b"\n").is_err()
    {
        return Probe::Unknown;
    }

    let mut reply = Vec::new();
    let mut chunk = [0_u8; 512];
    loop {
        if let Some(end) = reply.iter().position(|byte| *byte == b'\n') {
            return Probe::Line(String::from_utf8_lossy(&reply[..end]).into_owned());
        }
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
        else {
            return Probe::Unknown;
        };
        match stream.set_read_timeout(Some(remaining)) {
            // macOS rejects a timeout on a socket the peer already closed
            // (EINVAL); the read below then reports the close.
            Err(error) if error.kind() != io::ErrorKind::InvalidInput => return Probe::Unknown,
            _ => {}
        }
        match stream.read(&mut chunk) {
            // A close before a full line is a garbled reply.
            Ok(0) | Err(_) => return Probe::Unknown,
            Ok(read) => reply.extend_from_slice(&chunk[..read]),
        }
        if reply.len() > MAX_REPLY_BYTES {
            return Probe::Unknown;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_legacy_record_and_ignores_unknown_fields() {
        let bytes = br#"{"pid":42,"hyperd_endpoint":"127.0.0.1:1","health_port":7485,
            "started_at":"t","version":"0.7.0","identity":{"anything":1}}"#;
        assert_eq!(
            LegacyRecord::parse(bytes),
            Some(LegacyRecord {
                pid: 42,
                health_port: 7485
            })
        );
    }

    #[test]
    fn rejects_the_current_shape_and_unusable_records() {
        let current = br#"{"pid":42,"health_endpoint":"/x/daemon.sock","health_port":7485}"#;
        assert_eq!(LegacyRecord::parse(current), None);
        assert_eq!(LegacyRecord::parse(br#"{"pid":42,"health_port":0}"#), None);
        assert_eq!(LegacyRecord::parse(br#"{"pid":42}"#), None);
        assert_eq!(LegacyRecord::parse(b"not json"), None);
        assert_eq!(
            LegacyRecord::parse(br#"{"pid":42,"health_port":70000}"#),
            None
        );
    }
}
