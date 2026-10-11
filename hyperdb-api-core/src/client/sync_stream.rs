// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Sync stream abstraction for multiple transport types.
//!
//! This module provides [`SyncStream`], an enum that can hold different
//! sync stream types (TCP, TLS over TCP, Unix Domain Socket, named pipe)
//! while implementing the necessary I/O traits.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use rustls::{ClientConnection, StreamOwned};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(windows)]
use std::fs::File;

/// A sync stream over TCP, TLS over TCP, a Unix Domain Socket or a named pipe.
///
/// This enum provides a unified interface for different transport mechanisms,
/// allowing [`Client`](crate::client::Client) to work with all of them
/// transparently.
#[derive(Debug)]
pub enum SyncStream {
    /// TCP stream for network connections.
    Tcp(TcpStream),

    /// TLS-encrypted TCP stream, after a successful `SSLRequest` handshake.
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),

    /// Unix Domain Socket stream for local IPC (Unix only).
    #[cfg(unix)]
    Unix(UnixStream),

    /// Windows Named Pipe stream for local IPC (Windows only).
    #[cfg(windows)]
    NamedPipe(File),
}

impl SyncStream {
    /// Creates a new TCP stream wrapper.
    #[must_use]
    pub fn tcp(stream: TcpStream) -> Self {
        SyncStream::Tcp(stream)
    }

    /// Creates a new Unix Domain Socket stream wrapper.
    #[cfg(unix)]
    #[must_use]
    pub fn unix(stream: UnixStream) -> Self {
        SyncStream::Unix(stream)
    }

    /// Returns true if this is a TCP stream, encrypted or not.
    #[must_use]
    pub fn is_tcp(&self) -> bool {
        matches!(self, SyncStream::Tcp(_) | SyncStream::Tls(_))
    }

    /// Returns true if this is a TLS-encrypted TCP stream.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        matches!(self, SyncStream::Tls(_))
    }

    /// Returns true if this is a Unix Domain Socket stream.
    #[cfg(unix)]
    #[must_use]
    pub fn is_unix(&self) -> bool {
        matches!(self, SyncStream::Unix(_))
    }

    /// Creates a new Windows Named Pipe stream wrapper.
    #[cfg(windows)]
    pub fn named_pipe(file: File) -> Self {
        SyncStream::NamedPipe(file)
    }

    /// Returns true if this is a Windows Named Pipe stream.
    #[cfg(windows)]
    pub fn is_named_pipe(&self) -> bool {
        matches!(self, SyncStream::NamedPipe(_))
    }

    /// Sets `TCP_NODELAY` option (only applicable for TCP streams).
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] from the underlying
    /// [`std::net::TcpStream::set_nodelay`] when the socket option
    /// cannot be applied. Unix-domain and named-pipe variants are
    /// no-ops that always return `Ok(())`.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match self {
            SyncStream::Tcp(stream) => stream.set_nodelay(nodelay),
            SyncStream::Tls(stream) => stream.sock.set_nodelay(nodelay),
            #[cfg(unix)]
            SyncStream::Unix(_) => Ok(()), // No-op for Unix sockets
            #[cfg(windows)]
            SyncStream::NamedPipe(_) => Ok(()), // No-op for Named Pipes
        }
    }

    /// Sets read timeout.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] from the underlying transport's
    /// `set_read_timeout` call. On Windows, the named-pipe variant is
    /// a no-op that always returns `Ok(())`.
    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        match self {
            SyncStream::Tcp(stream) => stream.set_read_timeout(dur),
            SyncStream::Tls(stream) => stream.sock.set_read_timeout(dur),
            #[cfg(unix)]
            SyncStream::Unix(stream) => stream.set_read_timeout(dur),
            #[cfg(windows)]
            SyncStream::NamedPipe(_) => Ok(()), // Named pipes don't support timeouts directly
        }
    }

    /// Sets write timeout.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] from the underlying transport's
    /// `set_write_timeout` call. On Windows, the named-pipe variant
    /// is a no-op that always returns `Ok(())`.
    pub fn set_write_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        match self {
            SyncStream::Tcp(stream) => stream.set_write_timeout(dur),
            SyncStream::Tls(stream) => stream.sock.set_write_timeout(dur),
            #[cfg(unix)]
            SyncStream::Unix(stream) => stream.set_write_timeout(dur),
            #[cfg(windows)]
            SyncStream::NamedPipe(_) => Ok(()), // Named pipes don't support timeouts directly
        }
    }

    /// Returns the local address for TCP streams, or a placeholder for Unix sockets.
    #[must_use]
    pub fn local_addr_string(&self) -> String {
        match self {
            SyncStream::Tcp(stream) => stream
                .local_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
            SyncStream::Tls(stream) => stream
                .sock
                .local_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
            #[cfg(unix)]
            SyncStream::Unix(stream) => stream
                .local_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| p.display().to_string()))
                .unwrap_or_else(|| "unix-socket".to_string()),
            #[cfg(windows)]
            SyncStream::NamedPipe(_) => "named-pipe".to_string(),
        }
    }

    /// Returns the peer address for TCP streams, or a placeholder for Unix sockets.
    #[must_use]
    pub fn peer_addr_string(&self) -> String {
        match self {
            SyncStream::Tcp(stream) => stream
                .peer_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
            SyncStream::Tls(stream) => stream
                .sock
                .peer_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
            #[cfg(unix)]
            SyncStream::Unix(stream) => stream
                .peer_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| p.display().to_string()))
                .unwrap_or_else(|| "unix-socket".to_string()),
            #[cfg(windows)]
            SyncStream::NamedPipe(_) => "named-pipe".to_string(),
        }
    }
}

impl Read for SyncStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            SyncStream::Tcp(stream) => stream.read(buf),
            // rustls reports a peer that closes the socket without a
            // `close_notify` alert as `UnexpectedEof`. Report it as EOF, as
            // the plaintext stream would: the wire protocol is length-framed,
            // so a truncated message is still caught by the reader.
            SyncStream::Tls(stream) => match stream.read(buf) {
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
            #[cfg(unix)]
            SyncStream::Unix(stream) => stream.read(buf),
            #[cfg(windows)]
            SyncStream::NamedPipe(file) => file.read(buf),
        }
    }
}

impl Write for SyncStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            SyncStream::Tcp(stream) => stream.write(buf),
            SyncStream::Tls(stream) => stream.write(buf),
            #[cfg(unix)]
            SyncStream::Unix(stream) => stream.write(buf),
            #[cfg(windows)]
            SyncStream::NamedPipe(file) => file.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            SyncStream::Tcp(stream) => stream.flush(),
            SyncStream::Tls(stream) => stream.flush(),
            #[cfg(unix)]
            SyncStream::Unix(stream) => stream.flush(),
            #[cfg(windows)]
            SyncStream::NamedPipe(file) => file.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[expect(
        clippy::assertions_on_constants,
        reason = "compile-time invariant check kept as an assert for readability at the call site"
    )]
    #[test]
    fn test_sync_stream_variants_exist() {
        // We can't easily create streams without connecting,
        // so we just verify the module compiles correctly
        assert!(true);
    }
}
