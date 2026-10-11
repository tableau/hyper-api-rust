// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The per-user control transport of the daemon.
//!
//! The daemon's health protocol (`PING`, `HEARTBEAT`, `STOP`, `STATUS`, ...)
//! used to listen on a loopback TCP port, which any local process of any user
//! could reach. This module replaces the port with an endpoint only the
//! current user can open:
//!
//! - **Unix:** a Unix domain socket at `<state dir>/daemon.sock`, mode `0600`,
//!   inside the already-private state directory.
//! - **Windows:** a named pipe `\\.\pipe\hyperdb-mcp-<pid>-<random>` whose
//!   DACL grants access to the current user's SID only, that rejects remote
//!   clients, and whose first instance is created with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE` so nothing can squat on the name.
//!
//! The endpoint is recorded in the discovery record as a plain string
//! ([`HealthEndpoint`]). Both transports expose the same blocking
//! [`ControlStream`] (`Read + Write` with a per-operation timeout) and the
//! same bounded-wait accept ([`ControlListener::accept_timeout`]), so the
//! accept loop in the health module keeps its shape.
//!
//! Clients also authenticate the server: on Unix the socket peer's uid must be
//! the current user's, on Windows the pipe server's process must run as the
//! current user; otherwise [`connect`] fails with `PermissionDenied`.
//!
//! A known limitation on Windows: a daemon started from an elevated prompt
//! may create a pipe that clients running without elevation cannot open.
//!
//! This module depends only on `std`, `libc` and `windows-sys`, so it can be
//! compiled for Windows on its own.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

/// How long a handler waits for one read or write on a connection.
pub const CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a daemon's control endpoint lives: a socket path on Unix, a pipe name
/// on Windows. This is the string stored in the discovery record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthEndpoint(String);

impl HealthEndpoint {
    /// The endpoint a new daemon should bind, for the given state directory.
    ///
    /// # Errors
    ///
    /// Fails when the endpoint cannot be represented: on Unix, a state
    /// directory path that is not valid UTF-8 or leaves a socket path longer
    /// than the platform allows (the message names `HYPERDB_STATE_DIR`).
    pub fn for_new_daemon(state_dir: &Path) -> io::Result<Self> {
        platform::endpoint_for_new_daemon(state_dir).map(Self)
    }

    /// Parse the endpoint string of a discovery record found in `state_dir`.
    /// `None` when it is not an endpoint a daemon of this state directory
    /// could have produced, so a tampered or foreign record (say, a path to
    /// some other program's socket) is never connected to.
    ///
    /// On Unix the record must equal the exact socket path of `state_dir`. On
    /// Windows a pipe name does not encode the state directory, so only the
    /// `\\.\pipe\hyperdb-mcp-` prefix and character set are checked; the
    /// protection there is the random pipe name, the owner-only DACL and the
    /// client's server-owner check in [`connect`]. The ancestors of
    /// `state_dir` are not examined on either platform.
    #[must_use]
    pub fn from_record(record: &str, state_dir: &Path) -> Option<Self> {
        platform::is_valid_endpoint(record, state_dir).then(|| Self(record.to_owned()))
    }

    /// The string form, for the discovery record.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HealthEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The server side of the control transport.
#[derive(Debug)]
pub struct ControlListener {
    inner: platform::Listener,
}

impl ControlListener {
    /// Bind the endpoint. Fails when something live already serves it.
    ///
    /// On Unix a leftover socket file from a dead daemon is removed first, but
    /// only after a connect to it is refused and only if it really is a
    /// socket; the caller must hold the [`super::lock::DaemonLock`].
    ///
    /// # Errors
    ///
    /// Fails if the endpoint is in use, is not a socket, or cannot be created
    /// or restricted to the current user.
    pub fn bind(endpoint: &HealthEndpoint) -> io::Result<Self> {
        platform::Listener::bind(endpoint.as_str()).map(|inner| Self { inner })
    }

    /// Wait up to `timeout` for a client. `Ok(None)` on timeout (or a
    /// spurious wakeup), so the caller can recheck its shutdown flag.
    ///
    /// # Errors
    ///
    /// Fails when the listener itself is broken.
    pub fn accept_timeout(&mut self, timeout: Duration) -> io::Result<Option<ControlStream>> {
        self.inner
            .accept_timeout(timeout)
            .map(|stream| stream.map(|inner| ControlStream { inner }))
    }

    /// Nudge a listener blocked in [`Self::accept_timeout`] by connecting to
    /// it and hanging up. Best effort: the accept loop also wakes on its own
    /// cadence.
    pub fn wake(endpoint: &HealthEndpoint) {
        drop(connect(endpoint, Duration::from_millis(250)));
    }
}

/// One accepted or connected control connection.
#[derive(Debug)]
pub struct ControlStream {
    inner: platform::Stream,
}

impl ControlStream {
    /// Bound every later read and write on this connection by `timeout`.
    ///
    /// # Errors
    ///
    /// Fails if the timeout cannot be applied or is zero.
    pub fn set_io_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        self.inner.set_io_timeout(timeout)
    }

    /// End the conversation: deliver everything written so far, then close.
    ///
    /// # Errors
    ///
    /// Fails if the final flush fails; a peer that already hung up is fine.
    pub fn finish(self) -> io::Result<()> {
        self.inner.finish()
    }
}

impl Read for ControlStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for ControlStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Connect to a daemon's control endpoint as a client.
///
/// `connect_timeout` bounds the wait for a busy Windows pipe; a Unix socket
/// connect completes or fails at once. The returned stream has
/// [`CONNECTION_IO_TIMEOUT`] applied.
///
/// # Errors
///
/// `NotFound` / `ConnectionRefused` mean no daemon is serving the endpoint;
/// `TimedOut` means a Windows pipe stayed busy for the whole timeout.
pub fn connect(endpoint: &HealthEndpoint, connect_timeout: Duration) -> io::Result<ControlStream> {
    let mut stream = ControlStream {
        inner: platform::connect(endpoint.as_str(), connect_timeout)?,
    };
    stream.set_io_timeout(CONNECTION_IO_TIMEOUT)?;
    Ok(stream)
}

#[cfg(unix)]
mod platform {
    use std::io::{self, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// File name of the socket inside the state directory.
    const SOCKET_FILE_NAME: &str = "daemon.sock";

    /// Size of `sockaddr_un.sun_path` on this platform, NUL included.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    const SUN_PATH_CAPACITY: usize = 104;
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )))]
    const SUN_PATH_CAPACITY: usize = 108;

    pub(super) fn endpoint_for_new_daemon(state_dir: &Path) -> io::Result<String> {
        let path = state_dir.join(SOCKET_FILE_NAME);
        let path = path.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the state directory path {} is not valid UTF-8, so it cannot hold the \
                     daemon control socket; set HYPERDB_STATE_DIR to another directory",
                    state_dir.display()
                ),
            )
        })?;
        check_path_length(path)?;
        Ok(path.to_owned())
    }

    pub(super) fn is_valid_endpoint(record: &str, state_dir: &Path) -> bool {
        Path::new(record) == state_dir.join(SOCKET_FILE_NAME) && record.len() < SUN_PATH_CAPACITY
    }

    fn check_path_length(path: &str) -> io::Result<()> {
        let usable = SUN_PATH_CAPACITY - 1;
        if path.len() > usable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the daemon control socket path {path} is {} bytes, longer than the {usable} \
                     a Unix socket path can hold; set HYPERDB_STATE_DIR to a shorter directory",
                    path.len()
                ),
            ));
        }
        Ok(())
    }

    #[derive(Debug)]
    pub(super) struct Listener {
        socket: UnixListener,
        path: PathBuf,
        /// `(device, inode)` of the socket file this listener created.
        identity: (u64, u64),
    }

    impl Listener {
        pub(super) fn bind(endpoint: &str) -> io::Result<Self> {
            check_path_length(endpoint)?;
            let path = PathBuf::from(endpoint);
            reclaim_stale_socket(&path)?;
            let listener = UnixListener::bind(&path)?;
            let created = std::fs::symlink_metadata(&path)?;
            // Constructed first so a failure below still removes the socket.
            let this = Self {
                socket: listener,
                path,
                identity: (created.dev(), created.ino()),
            };
            // The state directory is private (0700), which is the real
            // protection during the window between bind and this chmod.
            std::fs::set_permissions(&this.path, std::fs::Permissions::from_mode(0o600))?;
            // Non-blocking so a client that hangs up between the poll and the
            // accept cannot park the accept loop.
            this.socket.set_nonblocking(true)?;
            Ok(this)
        }

        pub(super) fn accept_timeout(&mut self, timeout: Duration) -> io::Result<Option<Stream>> {
            let mut fds = libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // The floor keeps a sub-millisecond timeout from spinning.
            let millis = i32::try_from(timeout.as_millis().max(1)).unwrap_or(i32::MAX);
            // SAFETY: `fds` is one valid, initialised `pollfd` that outlives
            // the call, and the count passed is 1.
            let ready = unsafe { libc::poll(&raw mut fds, 1, millis) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::Interrupted {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            if ready == 0 {
                return Ok(None);
            }
            match self.socket.accept() {
                Ok((stream, _addr)) => {
                    // An accepted socket can inherit the listener's
                    // non-blocking mode on some platforms.
                    stream.set_nonblocking(false)?;
                    Ok(Some(Stream { inner: stream }))
                }
                // A client that gave up between the poll and the accept, or a
                // signal, is not a broken listener.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                            | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    Ok(None)
                }
                Err(error) => Err(error),
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Best effort: a daemon killed outright leaves the file, and the
            // next bind reclaims it. Only unlink the file this listener
            // created, so a successor that already reclaimed the path (the
            // lock released before this drop) keeps its live socket.
            let still_ours = std::fs::symlink_metadata(&self.path)
                .is_ok_and(|now| (now.dev(), now.ino()) == self.identity);
            if still_ours {
                drop(std::fs::remove_file(&self.path));
            }
        }
    }

    /// Remove a leftover socket file so the path can be bound again.
    ///
    /// Refuses to touch anything that is not a socket, and refuses to unlink a
    /// socket something still accepts on.
    fn reclaim_stale_socket(path: &Path) -> io::Result<()> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists and is not a socket; refusing to replace it",
                    path.display()
                ),
            ));
        }
        match UnixStream::connect(path) {
            Ok(_live) => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another daemon is serving {}", path.display()),
            )),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn connect(endpoint: &str, _connect_timeout: Duration) -> io::Result<Stream> {
        let inner = UnixStream::connect(endpoint)?;
        // The socket path sits in a directory only we may write to, but a
        // stale record could still point at a socket some other user's
        // process owns; never speak to one.
        let peer = peer_uid(&inner)?;
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let ours = unsafe { libc::geteuid() };
        if peer != ours {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the daemon control socket is served by uid {peer}, not by this user"),
            ));
        }
        Ok(Stream { inner })
    }

    /// The effective uid of the process on the other end of `stream`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
            .expect("ucred size fits socklen_t");
        // SAFETY: the descriptor is valid for the call; `credentials` is a
        // writable `ucred` and `length` holds its size.
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut length,
            )
        };
        if status == 0 {
            Ok(credentials.uid)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// The effective uid of the process on the other end of `stream`.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: the descriptor is valid for the call; both out-pointers are
        // writable.
        let status = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
        if status == 0 {
            Ok(uid)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(test)]
    pub(super) fn own_peer_uid_matches(stream: &UnixStream) -> bool {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        peer_uid(stream).is_ok_and(|uid| uid == unsafe { libc::geteuid() })
    }

    #[derive(Debug)]
    pub(super) struct Stream {
        inner: UnixStream,
    }

    impl Stream {
        pub(super) fn set_io_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.inner.set_read_timeout(Some(timeout))?;
            self.inner.set_write_timeout(Some(timeout))
        }

        pub(super) fn finish(mut self) -> io::Result<()> {
            self.inner.flush()?;
            match self.inner.shutdown(std::net::Shutdown::Write) {
                // The peer read its answer and hung up first: nothing is lost.
                Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
                other => other,
            }
        }

        pub(super) fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buf)
        }

        pub(super) fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        pub(super) fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::OpenOptions;
    use std::hash::{BuildHasher, RandomState};
    use std::io;
    use std::mem;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::ptr;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING,
        ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
        GetLastError, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FlushFileBuffers, PIPE_ACCESS_DUPLEX,
        ReadFile, SECURITY_IDENTIFICATION, WriteFile,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken,
        PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    /// Prefix every endpoint of ours carries; anything else in a record is
    /// refused.
    const PIPE_PREFIX: &str = r"\\.\pipe\hyperdb-mcp-";

    /// Size of the in/out buffers of a pipe instance.
    const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

    /// How often a busy pipe is retried while connecting.
    const BUSY_RETRY: Duration = Duration::from_millis(10);

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the Unix twin can fail, and both share one signature"
    )]
    pub(super) fn endpoint_for_new_daemon(_state_dir: &Path) -> io::Result<String> {
        let random = RandomState::new().hash_one(std::process::id());
        Ok(format!("{PIPE_PREFIX}{}-{random:016x}", std::process::id()))
    }

    /// Accepts any pipe name with this daemon family's prefix and character
    /// set; `_state_dir` is deliberately not consulted. Unlike the Unix socket
    /// path, a pipe name does not encode the state directory, so the check
    /// cannot pin the exact endpoint. Protection on Windows is the pipe's DACL
    /// (owner only), not this check.
    pub(super) fn is_valid_endpoint(record: &str, _state_dir: &Path) -> bool {
        record.strip_prefix(PIPE_PREFIX).is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Milliseconds for the Win32 wait calls, never `INFINITE`.
    fn wait_millis(timeout: Duration) -> u32 {
        u32::try_from(timeout.as_millis()).map_or(u32::MAX - 1, |millis| millis.min(u32::MAX - 1))
    }

    fn zeroed_overlapped(event: &OwnedHandle) -> OVERLAPPED {
        // SAFETY: `OVERLAPPED` is a plain-old-data struct of integers and
        // pointers; all-zero is its documented initial state.
        let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
        overlapped.hEvent = event.as_raw_handle();
        overlapped
    }

    fn new_event() -> io::Result<OwnedHandle> {
        // SAFETY: null attributes and name are allowed; the returned handle is
        // checked for null before it is wrapped.
        let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a valid, owned event handle returned just above.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    /// A security descriptor allowing only the current user, freed on drop.
    struct UserOnlyDescriptor {
        descriptor: *mut core::ffi::c_void,
    }

    // SAFETY: the descriptor is an immutable, self-contained `LocalAlloc`
    // block; nothing ties it to the creating thread.
    unsafe impl Send for UserOnlyDescriptor {}

    impl UserOnlyDescriptor {
        fn new() -> io::Result<Self> {
            let sddl = wide(&format!("D:P(A;;GA;;;{})", current_user_sid()?));
            let mut descriptor = ptr::null_mut();
            // SAFETY: `sddl` is NUL-terminated and outlives the call;
            // `descriptor` is a valid out-pointer; the size out-parameter is
            // optional.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &raw mut descriptor,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { descriptor })
        }

        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: u32::try_from(mem::size_of::<SECURITY_ATTRIBUTES>())
                    .expect("SECURITY_ATTRIBUTES is a few bytes"),
                lpSecurityDescriptor: self.descriptor,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for UserOnlyDescriptor {
        fn drop(&mut self) {
            // SAFETY: `descriptor` was allocated by the conversion call with
            // `LocalAlloc` and is freed exactly once, here.
            unsafe { LocalFree(self.descriptor) };
        }
    }

    /// The current user's SID in string form (`S-1-5-21-...`).
    fn current_user_sid() -> io::Result<String> {
        // SAFETY: the pseudo-handle of the current process needs no closing.
        token_user_sid(unsafe { GetCurrentProcess() })
    }

    /// The user SID, in string form, of the process behind `process`.
    fn token_user_sid(process: windows_sys::Win32::Foundation::HANDLE) -> io::Result<String> {
        let mut token = ptr::null_mut();
        // SAFETY: `process` is a valid process handle with query access;
        // `token` is a valid out-pointer.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `token` is a valid handle returned just above, owned here.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };

        let mut needed = 0_u32;
        // SAFETY: a null buffer of length 0 is the documented way to ask for
        // the required size.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                ptr::null_mut(),
                0,
                &raw mut needed,
            );
        }
        // SAFETY: reads the thread's last-error value only.
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::last_os_error());
        }
        // `u64` elements keep the buffer aligned for the pointers in TOKEN_USER.
        let words = (usize::try_from(needed).expect("u32 fits usize")).div_ceil(8);
        let mut buffer = vec![0_u64; words];
        // SAFETY: `buffer` holds at least `needed` writable bytes.
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call above filled `buffer` with a TOKEN_USER, and the
        // buffer is 8-byte aligned.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let mut text = ptr::null_mut();
        // SAFETY: `sid` points into `buffer`, alive for the call; `text` is a
        // valid out-pointer.
        if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut length = 0;
        // SAFETY: on success `text` is a NUL-terminated wide string.
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `text` holds `length` initialised `u16`s, per the loop above.
        let sid_text =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
        // SAFETY: `text` was allocated by `ConvertSidToStringSidW` with
        // `LocalAlloc` and is freed once, after its last use above.
        unsafe { LocalFree(text.cast()) };
        Ok(sid_text)
    }

    /// Create one instance of the pipe, restricted to the current user.
    fn create_instance(
        name: &[u16],
        security: &UserOnlyDescriptor,
        first: bool,
    ) -> io::Result<OwnedHandle> {
        let attributes = security.attributes();
        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: `name` is NUL-terminated; `attributes` and the descriptor it
        // points to outlive the call.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                0,
                &raw const attributes,
            )
        };
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a valid pipe handle returned just above.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    pub(super) struct Listener {
        name: Vec<u16>,
        security: UserOnlyDescriptor,
        /// The instance clients are currently being accepted on.
        pipe: OwnedHandle,
        event: OwnedHandle,
        /// Boxed so its address is stable while a connect is pending.
        overlapped: Box<OVERLAPPED>,
        connecting: bool,
    }

    impl std::fmt::Debug for Listener {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Listener")
                .field("connecting", &self.connecting)
                .finish_non_exhaustive()
        }
    }

    // SAFETY: the raw pointers inside `OVERLAPPED` are only touched through
    // `&mut self`, and the kernel does not care which thread owns the handle.
    unsafe impl Send for Listener {}

    impl Listener {
        pub(super) fn bind(endpoint: &str) -> io::Result<Self> {
            let name = wide(endpoint);
            let security = UserOnlyDescriptor::new()?;
            let pipe = create_instance(&name, &security, true)?;
            let event = new_event()?;
            let overlapped = Box::new(zeroed_overlapped(&event));
            Ok(Self {
                name,
                security,
                pipe,
                event,
                overlapped,
                connecting: false,
            })
        }

        pub(super) fn accept_timeout(&mut self, timeout: Duration) -> io::Result<Option<Stream>> {
            let connected = if self.connecting {
                self.wait_for_connect(timeout)?
            } else {
                *self.overlapped = zeroed_overlapped(&self.event);
                // SAFETY: the pipe handle is valid, and the boxed OVERLAPPED
                // stays at a fixed address until the operation completes or
                // is cancelled in `Drop`.
                let ok = unsafe {
                    ConnectNamedPipe(self.pipe.as_raw_handle(), &raw mut *self.overlapped)
                };
                if ok != 0 {
                    true
                } else {
                    // SAFETY: reads the thread's last-error value only.
                    match unsafe { GetLastError() } {
                        // A client that connected and hung up before this call
                        // leaves the instance in the connected state; the first
                        // read on it reports end of stream.
                        ERROR_PIPE_CONNECTED | ERROR_NO_DATA => true,
                        ERROR_IO_PENDING => {
                            self.connecting = true;
                            self.wait_for_connect(timeout)?
                        }
                        _ => return Err(io::Error::last_os_error()),
                    }
                }
            };
            if !connected {
                return Ok(None);
            }
            // Create the next instance before handing this one off, so the
            // name never has a moment without a listening instance.
            let next = create_instance(&self.name, &self.security, false)?;
            let accepted = mem::replace(&mut self.pipe, next);
            Ok(Some(Stream::new(accepted)?))
        }

        /// Wait for the pending connect. `true` once a client is connected.
        fn wait_for_connect(&mut self, timeout: Duration) -> io::Result<bool> {
            // SAFETY: the event handle is valid.
            let wait =
                unsafe { WaitForSingleObject(self.event.as_raw_handle(), wait_millis(timeout)) };
            match wait {
                WAIT_TIMEOUT => Ok(false),
                WAIT_OBJECT_0 => {
                    self.connecting = false;
                    let mut transferred = 0_u32;
                    // SAFETY: the connect on this pipe and OVERLAPPED has
                    // completed; the result is read without waiting.
                    let ok = unsafe {
                        GetOverlappedResult(
                            self.pipe.as_raw_handle(),
                            &raw const *self.overlapped,
                            &raw mut transferred,
                            0,
                        )
                    };
                    if ok == 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(true)
                    }
                }
                _ => Err(io::Error::last_os_error()),
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            if self.connecting {
                let mut transferred = 0_u32;
                // SAFETY: cancels and then waits out the one pending connect
                // so the kernel is finished with the boxed OVERLAPPED before
                // it is freed; the handles are still open here.
                unsafe {
                    CancelIoEx(self.pipe.as_raw_handle(), &raw const *self.overlapped);
                    GetOverlappedResult(
                        self.pipe.as_raw_handle(),
                        &raw const *self.overlapped,
                        &raw mut transferred,
                        1,
                    );
                }
            }
        }
    }

    /// Refuse a pipe whose server process does not run as the current user.
    /// The pipe name is random and its DACL owner-only, but a stale record
    /// could name a pipe another user's process created; never speak to one.
    fn verify_server_is_us(pipe: &OwnedHandle) -> io::Result<()> {
        let mut server_pid = 0_u32;
        // SAFETY: `pipe` is a valid pipe handle; `server_pid` is writable.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &raw mut server_pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: plain call; a null handle on failure is checked below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, server_pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `process` is a valid handle returned just above, owned here.
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        if token_user_sid(process.as_raw_handle())? == current_user_sid()? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the daemon control pipe is served by a process of another user",
            ))
        }
    }

    pub(super) fn connect(endpoint: &str, connect_timeout: Duration) -> io::Result<Stream> {
        let deadline = Instant::now() + connect_timeout;
        loop {
            let opened = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                // The server may identify us but never impersonate us.
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(endpoint);
            match opened {
                Ok(file) => {
                    let pipe = OwnedHandle::from(file);
                    verify_server_is_us(&pipe)?;
                    return Stream::new(pipe);
                }
                Err(error)
                    if error
                        .raw_os_error()
                        .and_then(|code| u32::try_from(code).ok())
                        == Some(ERROR_PIPE_BUSY) =>
                {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the daemon control pipe stayed busy",
                        ));
                    }
                    std::thread::sleep(BUSY_RETRY);
                }
                Err(error)
                    if error
                        .raw_os_error()
                        .and_then(|code| u32::try_from(code).ok())
                        == Some(ERROR_FILE_NOT_FOUND) =>
                {
                    return Err(io::Error::new(io::ErrorKind::NotFound, error));
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[derive(Debug)]
    pub(super) struct Stream {
        pipe: OwnedHandle,
        event: OwnedHandle,
        timeout: Duration,
    }

    impl Stream {
        fn new(pipe: OwnedHandle) -> io::Result<Self> {
            Ok(Self {
                pipe,
                event: new_event()?,
                timeout: super::CONNECTION_IO_TIMEOUT,
            })
        }

        pub(super) fn set_io_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            if timeout.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a zero I/O timeout is not allowed",
                ));
            }
            self.timeout = timeout;
            Ok(())
        }

        /// Wait for the overlapped operation started by `ReadFile`/`WriteFile`
        /// and return the byte count; cancel it on timeout.
        fn complete(&mut self, started: i32, overlapped: &mut OVERLAPPED) -> io::Result<usize> {
            if started == 0 {
                // SAFETY: reads the thread's last-error value only.
                let code = unsafe { GetLastError() };
                if code != ERROR_IO_PENDING {
                    return Err(io::Error::from_raw_os_error(
                        i32::try_from(code).unwrap_or(i32::MAX),
                    ));
                }
            }
            // SAFETY: the event handle is valid.
            let wait = unsafe {
                WaitForSingleObject(self.event.as_raw_handle(), wait_millis(self.timeout))
            };
            let mut transferred = 0_u32;
            if wait != WAIT_OBJECT_0 {
                // Captured before the cancel call can overwrite it.
                let wait_error = (wait != WAIT_TIMEOUT).then(io::Error::last_os_error);
                // SAFETY: cancels the one operation on this handle that uses
                // `overlapped`, then waits for the kernel to let go of it, so
                // neither the OVERLAPPED nor the caller's buffer is freed
                // while the I/O is still in flight.
                let finished = unsafe {
                    CancelIoEx(self.pipe.as_raw_handle(), &raw const *overlapped);
                    GetOverlappedResult(
                        self.pipe.as_raw_handle(),
                        &raw const *overlapped,
                        &raw mut transferred,
                        1,
                    )
                };
                if finished != 0 {
                    // The operation completed before the cancel took effect.
                    return Ok(usize::try_from(transferred).expect("u32 fits usize"));
                }
                return Err(wait_error.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "the control connection timed out")
                }));
            }
            // SAFETY: the operation has completed; read its result without
            // waiting.
            let ok = unsafe {
                GetOverlappedResult(
                    self.pipe.as_raw_handle(),
                    &raw const *overlapped,
                    &raw mut transferred,
                    0,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(usize::try_from(transferred).expect("u32 fits usize"))
        }

        /// A closed peer reads as end of stream, like a socket.
        fn is_closed_peer(error: &io::Error) -> bool {
            error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok())
                .is_some_and(|code| {
                    matches!(
                        code,
                        ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED
                    )
                })
        }

        pub(super) fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let length = u32::try_from(buf.len()).unwrap_or(u32::MAX);
            let mut overlapped = zeroed_overlapped(&self.event);
            // SAFETY: `buf` and `overlapped` live until `complete` has seen
            // the operation finish or cancelled it.
            let started = unsafe {
                ReadFile(
                    self.pipe.as_raw_handle(),
                    buf.as_mut_ptr(),
                    length,
                    ptr::null_mut(),
                    &raw mut overlapped,
                )
            };
            match self.complete(started, &mut overlapped) {
                Err(error) if Self::is_closed_peer(&error) => Ok(0),
                other => other,
            }
        }

        pub(super) fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let length = u32::try_from(buf.len()).unwrap_or(u32::MAX);
            let mut overlapped = zeroed_overlapped(&self.event);
            // SAFETY: as in `read`.
            let started = unsafe {
                WriteFile(
                    self.pipe.as_raw_handle(),
                    buf.as_ptr(),
                    length,
                    ptr::null_mut(),
                    &raw mut overlapped,
                )
            };
            match self.complete(started, &mut overlapped) {
                Err(error) if Self::is_closed_peer(&error) => {
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                }
                other => other,
            }
        }

        #[expect(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "writes are complete when `write` returns; shared signature with Unix"
        )]
        pub(super) fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        /// Close once the peer has read everything written.
        ///
        /// `FlushFileBuffers` waits for the peer to drain the pipe and cannot
        /// be bounded, so it runs on a short-lived thread: a peer that never
        /// reads pins that one idle thread, not the connection handler.
        pub(super) fn finish(self) -> io::Result<()> {
            let Self { pipe, event, .. } = self;
            drop(event);
            std::thread::Builder::new()
                .name("hyperdb-control-flush".to_owned())
                .spawn(move || {
                    // SAFETY: the pipe handle is valid and owned by this
                    // thread until it is dropped below.
                    unsafe { FlushFileBuffers(pipe.as_raw_handle()) };
                    drop(pipe);
                })
                .map(drop)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::time::{Duration, Instant};

    use super::{ControlListener, ControlStream, HealthEndpoint, connect};

    fn endpoint_in(dir: &tempfile::TempDir) -> HealthEndpoint {
        HealthEndpoint::for_new_daemon(dir.path()).expect("endpoint")
    }

    fn accept_one(listener: &mut ControlListener) -> ControlStream {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(stream) = listener
                .accept_timeout(Duration::from_millis(100))
                .expect("accept")
            {
                return stream;
            }
        }
        panic!("no client arrived");
    }

    #[test]
    fn a_line_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(&dir);
        let mut listener = ControlListener::bind(&endpoint).expect("bind");
        let server = std::thread::spawn(move || {
            let stream = accept_one(&mut listener);
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read");
            assert_eq!(line, "PING\n");
            let stream = reader.get_mut();
            stream.write_all(b"PONG\n").expect("write");
            let stream = reader.into_inner();
            stream.finish().expect("finish");
            listener
        });
        let mut client = connect(&endpoint, Duration::from_secs(5)).expect("connect");
        client.write_all(b"PING\n").expect("send");
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).expect("reply");
        assert_eq!(reply, "PONG\n");
        drop(server.join().expect("server thread"));
    }

    #[test]
    fn accept_times_out_without_a_client() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut listener = ControlListener::bind(&endpoint_in(&dir)).expect("bind");
        let started = Instant::now();
        assert!(
            listener
                .accept_timeout(Duration::from_millis(100))
                .expect("accept")
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn wake_unblocks_nothing_and_does_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(&dir);
        let mut listener = ControlListener::bind(&endpoint).expect("bind");
        ControlListener::wake(&endpoint);
        // The wake connection is accepted like any other.
        let _hung_up = accept_one(&mut listener);
    }

    #[test]
    fn a_second_bind_on_a_live_endpoint_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(&dir);
        let mut first = ControlListener::bind(&endpoint).expect("bind");
        assert!(
            ControlListener::bind(&endpoint).is_err(),
            "a live endpoint must not be taken over"
        );
        // The first listener still serves.
        let client = connect(&endpoint, Duration::from_secs(5)).expect("connect to the survivor");
        let _server_side = accept_one(&mut first);
        drop(client);
    }

    #[test]
    fn connecting_to_nothing_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(&dir);
        assert!(connect(&endpoint, Duration::from_millis(200)).is_err());
    }

    #[test]
    fn a_silent_peer_hits_the_read_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(&dir);
        let mut listener = ControlListener::bind(&endpoint).expect("bind");
        let _silent_client = connect(&endpoint, Duration::from_secs(5)).expect("connect");
        let mut server_side = accept_one(&mut listener);
        server_side
            .set_io_timeout(Duration::from_millis(150))
            .expect("timeout");
        let started = Instant::now();
        let error = server_side
            .read(&mut [0_u8; 8])
            .expect_err("a silent peer must time out");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ),
            "unexpected error kind: {error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_hung_up_peer_reads_as_end_of_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(&dir);
        let mut listener = ControlListener::bind(&endpoint).expect("bind");
        let client = connect(&endpoint, Duration::from_secs(5)).expect("connect");
        let mut server_side = accept_one(&mut listener);
        drop(client);
        assert_eq!(server_side.read(&mut [0_u8; 8]).expect("read"), 0);
    }

    #[test]
    fn record_parsing_round_trips_and_rejects_foreign_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(&dir);
        assert_eq!(
            HealthEndpoint::from_record(endpoint.as_str(), dir.path()),
            Some(endpoint.clone())
        );
        assert_eq!(endpoint.to_string(), endpoint.as_str());
        assert!(HealthEndpoint::from_record("", dir.path()).is_none());
        assert!(HealthEndpoint::from_record("127.0.0.1:7485", dir.path()).is_none());
        assert!(HealthEndpoint::from_record("a\0b", dir.path()).is_none());
        #[cfg(unix)]
        assert!(
            HealthEndpoint::from_record("/var/run/docker.sock", dir.path()).is_none(),
            "a record naming another program's socket must be refused"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_records_accept_our_pipe_prefix_and_refuse_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(HealthEndpoint::from_record(r"\\.\pipe\hyperdb-mcp-12-00ab", dir.path()).is_some());
        assert!(HealthEndpoint::from_record(r"\\.\pipe\docker_engine", dir.path()).is_none());
        assert!(HealthEndpoint::from_record(r"\\.\pipe\hyperdb-mcp-", dir.path()).is_none());
        assert!(HealthEndpoint::from_record(r"\\.\pipe\hyperdb-mcp-a\b", dir.path()).is_none());
        assert!(HealthEndpoint::from_record(r"\\server\pipe\hyperdb-mcp-1", dir.path()).is_none());
    }

    #[cfg(unix)]
    mod unix {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        use super::{ControlListener, endpoint_in};
        use crate::daemon::control::HealthEndpoint;

        #[test]
        fn the_socket_is_owner_only() {
            let dir = tempfile::tempdir().expect("tempdir");
            let endpoint = endpoint_in(&dir);
            let _listener = ControlListener::bind(&endpoint).expect("bind");
            let mode = std::fs::metadata(endpoint.as_str())
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        /// The peer-uid check passes for a server running as this user (the
        /// only kind this test can create without a second account).
        #[test]
        fn the_peer_uid_of_a_same_user_server_is_accepted() {
            let dir = tempfile::tempdir().expect("tempdir");
            let endpoint = endpoint_in(&dir);
            let _listener = ControlListener::bind(&endpoint).expect("bind");
            let stream =
                std::os::unix::net::UnixStream::connect(endpoint.as_str()).expect("connect");
            assert!(crate::daemon::control::platform::own_peer_uid_matches(
                &stream
            ));
            crate::daemon::control::connect(&endpoint, std::time::Duration::from_secs(5))
                .expect("a same-user server passes the peer check");
        }

        #[test]
        fn a_stale_socket_file_is_reclaimed() {
            let dir = tempfile::tempdir().expect("tempdir");
            let endpoint = endpoint_in(&dir);
            // A std listener leaves its socket file behind when dropped,
            // which is exactly what a killed daemon does.
            drop(UnixListener::bind(endpoint.as_str()).expect("make stale socket"));
            assert!(std::path::Path::new(endpoint.as_str()).exists());
            ControlListener::bind(&endpoint).expect("a stale socket is reclaimed");
        }

        #[test]
        fn a_regular_file_at_the_path_is_refused() {
            let dir = tempfile::tempdir().expect("tempdir");
            let endpoint = endpoint_in(&dir);
            std::fs::write(endpoint.as_str(), b"precious").expect("write");
            let error = ControlListener::bind(&endpoint).expect_err("must refuse");
            assert!(error.to_string().contains("not a socket"), "{error}");
            assert_eq!(std::fs::read(endpoint.as_str()).expect("read"), b"precious");
        }

        #[test]
        fn the_socket_file_goes_away_with_the_listener() {
            let dir = tempfile::tempdir().expect("tempdir");
            let endpoint = endpoint_in(&dir);
            drop(ControlListener::bind(&endpoint).expect("bind"));
            assert!(!std::path::Path::new(endpoint.as_str()).exists());
        }

        #[test]
        fn dropping_an_old_listener_keeps_a_successors_socket() {
            let dir = tempfile::tempdir().expect("tempdir");
            let endpoint = endpoint_in(&dir);
            let old = ControlListener::bind(&endpoint).expect("bind old");
            // The old daemon's file is gone and a successor reclaimed the path.
            std::fs::remove_file(endpoint.as_str()).expect("unlink");
            let _successor = ControlListener::bind(&endpoint).expect("bind successor");
            drop(old);
            assert!(
                std::path::Path::new(endpoint.as_str()).exists(),
                "the successor's live socket must survive the old listener's drop"
            );
        }

        #[test]
        fn an_over_long_path_names_the_state_dir_variable() {
            let dir = tempfile::tempdir().expect("tempdir");
            let long = dir.path().join("x".repeat(120));
            let error = HealthEndpoint::for_new_daemon(&long).expect_err("too long");
            assert!(error.to_string().contains("HYPERDB_STATE_DIR"), "{error}");
        }
    }
}
