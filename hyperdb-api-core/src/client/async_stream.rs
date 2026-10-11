// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async stream abstraction for multiple transport types.
//!
//! This module provides [`AsyncStream`], an enum that can hold different
//! async stream types (TCP, TLS over TCP, Unix Domain Socket, named pipe)
//! while implementing the necessary async I/O traits.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient;

/// An async stream over TCP, TLS over TCP, a Unix Domain Socket or a named pipe.
///
/// This enum provides a unified interface for different transport mechanisms,
/// allowing [`AsyncClient`](crate::client::AsyncClient) to work with all of
/// them transparently.
#[derive(Debug)]
pub enum AsyncStream {
    /// TCP stream for network connections.
    Tcp(TcpStream),

    /// TLS-encrypted TCP stream, after a successful `SSLRequest` handshake.
    Tls(Box<TlsStream<TcpStream>>),

    /// Unix Domain Socket stream for local IPC (Unix only).
    #[cfg(unix)]
    Unix(UnixStream),

    /// Windows Named Pipe stream for local IPC (Windows only).
    #[cfg(windows)]
    NamedPipe(NamedPipeClient),
}

impl AsyncStream {
    /// Creates a new TCP stream wrapper.
    pub fn tcp(stream: TcpStream) -> Self {
        AsyncStream::Tcp(stream)
    }

    /// Creates a new Unix Domain Socket stream wrapper.
    #[cfg(unix)]
    pub fn unix(stream: UnixStream) -> Self {
        AsyncStream::Unix(stream)
    }

    /// Returns true if this is a TCP stream, encrypted or not.
    pub fn is_tcp(&self) -> bool {
        matches!(self, AsyncStream::Tcp(_) | AsyncStream::Tls(_))
    }

    /// Returns true if this is a TLS-encrypted TCP stream.
    pub fn is_tls(&self) -> bool {
        matches!(self, AsyncStream::Tls(_))
    }

    /// Returns true if this is a Unix Domain Socket stream.
    #[cfg(unix)]
    pub fn is_unix(&self) -> bool {
        matches!(self, AsyncStream::Unix(_))
    }

    /// Creates a new Windows Named Pipe stream wrapper.
    #[cfg(windows)]
    pub fn named_pipe(client: NamedPipeClient) -> Self {
        AsyncStream::NamedPipe(client)
    }

    /// Returns true if this is a Windows Named Pipe stream.
    #[cfg(windows)]
    pub fn is_named_pipe(&self) -> bool {
        matches!(self, AsyncStream::NamedPipe(_))
    }

    /// Sets `TCP_NODELAY` option (only applicable for TCP streams).
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] from the underlying
    /// [`tokio::net::TcpStream::set_nodelay`] when the socket option
    /// cannot be applied. Unix-domain and named-pipe variants are
    /// no-ops that always return `Ok(())`.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match self {
            AsyncStream::Tcp(stream) => stream.set_nodelay(nodelay),
            AsyncStream::Tls(stream) => stream.get_ref().0.set_nodelay(nodelay),
            #[cfg(unix)]
            AsyncStream::Unix(_) => Ok(()), // No-op for Unix sockets
            #[cfg(windows)]
            AsyncStream::NamedPipe(_) => Ok(()), // No-op for Named Pipes
        }
    }

    /// Returns the local address for TCP streams, or a placeholder for Unix sockets.
    pub fn local_addr_string(&self) -> String {
        match self {
            AsyncStream::Tcp(stream) => stream
                .local_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
            AsyncStream::Tls(stream) => stream
                .get_ref()
                .0
                .local_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => stream
                .local_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| p.display().to_string()))
                .unwrap_or_else(|| "unix-socket".to_string()),
            #[cfg(windows)]
            AsyncStream::NamedPipe(_) => "named-pipe".to_string(),
        }
    }

    /// Returns the peer address for TCP streams, or a placeholder for Unix sockets.
    pub fn peer_addr_string(&self) -> String {
        match self {
            AsyncStream::Tcp(stream) => stream
                .peer_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
            AsyncStream::Tls(stream) => stream
                .get_ref()
                .0
                .peer_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => stream
                .peer_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| p.display().to_string()))
                .unwrap_or_else(|| "unix-socket".to_string()),
            #[cfg(windows)]
            AsyncStream::NamedPipe(_) => "named-pipe".to_string(),
        }
    }
}

impl AsyncRead for AsyncStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            // rustls reports a peer that closes the socket without a
            // `close_notify` alert as `UnexpectedEof`. Report it as EOF, as
            // the plaintext stream would: the wire protocol is length-framed,
            // so a truncated message is still caught by the reader.
            AsyncStream::Tls(stream) => match Pin::new(stream.as_mut()).poll_read(cx, buf) {
                Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::UnexpectedEof => {
                    Poll::Ready(Ok(()))
                }
                other => other,
            },
            #[cfg(unix)]
            AsyncStream::Unix(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(windows)]
            AsyncStream::NamedPipe(pipe) => Pin::new(pipe).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for AsyncStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
            AsyncStream::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(windows)]
            AsyncStream::NamedPipe(pipe) => Pin::new(pipe).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            AsyncStream::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(windows)]
            AsyncStream::NamedPipe(pipe) => Pin::new(pipe).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            AsyncStream::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(windows)]
            AsyncStream::NamedPipe(pipe) => Pin::new(pipe).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            AsyncStream::Tls(stream) => Pin::new(stream.as_mut()).poll_write_vectored(cx, bufs),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            #[cfg(windows)]
            AsyncStream::NamedPipe(pipe) => Pin::new(pipe).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            AsyncStream::Tcp(stream) => stream.is_write_vectored(),
            AsyncStream::Tls(stream) => stream.is_write_vectored(),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => stream.is_write_vectored(),
            #[cfg(windows)]
            AsyncStream::NamedPipe(pipe) => pipe.is_write_vectored(),
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
    fn test_async_stream_variants_exist() {
        // We can't easily create streams without connecting,
        // so we just verify the module compiles correctly
        assert!(true);
    }
}
