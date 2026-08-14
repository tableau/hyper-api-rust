// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cross-platform resolution for the persistent-database default path.
//!
//! The persistent database lives in the platform-standard data directory:
//!
//! - **macOS:** `~/Library/Application Support/hyperdb/workspace.hyper`
//! - **Linux:** `$XDG_DATA_HOME/hyperdb/workspace.hyper`
//!   (defaults to `~/.local/share/hyperdb/workspace.hyper`)
//! - **Windows:** `%APPDATA%\hyperdb\workspace.hyper`
//!
//! Note this is intentionally distinct from `~/.hyperdb/`, which is the
//! daemon's state directory (`daemon.json`, `logs/`). Daemon coordination
//! and user data have different lifecycles, so they live in different
//! places.
//!
//! Resolution precedence:
//! 1. Explicit CLI value (`--persistent-db <PATH>` or the deprecated
//!    `--workspace <PATH>`).
//! 2. `HYPERDB_PERSISTENT_DB` environment variable.
//! 3. Platform default via [`dirs::data_dir`].

use std::ffi::OsStr;
use std::path::PathBuf;

use serde::Serialize;

/// Application directory name used inside the platform data dir.
const APP_DIR_NAME: &str = "hyperdb";

/// Filename of the persistent workspace inside the app dir.
const PERSISTENT_DB_FILENAME: &str = "workspace.hyper";

/// Environment variable that overrides the platform-default path.
pub const ENV_PERSISTENT_DB: &str = "HYPERDB_PERSISTENT_DB";

/// The winning input in persistent-database path resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistentDbPathSource {
    /// The preferred `--persistent-db` CLI flag.
    Cli,
    /// The deprecated `--workspace` CLI alias.
    DeprecatedAlias,
    /// The `HYPERDB_PERSISTENT_DB` environment variable.
    Environment,
    /// The platform data-directory default.
    PlatformDefault,
    /// Persistent storage was explicitly disabled with `--ephemeral-only`.
    Disabled,
}

/// A persistent-database resolution result that retains its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPersistentDbPath {
    /// Effective UTF-8-compatible runtime path after lossy conversion and
    /// literal home-prefix expansion, absent only when persistence is disabled
    /// or no platform data directory is available.
    pub path: Option<PathBuf>,
    /// Exact operating-system path supplied by the winning source, before the
    /// runtime's lossy string conversion and literal home-prefix expansion.
    pub observed_path: Option<PathBuf>,
    /// The source that won precedence resolution.
    pub source: PersistentDbPathSource,
}

/// Returns the platform-default path for the persistent database. Returns
/// `None` if the home / data directory cannot be determined (rare; usually
/// indicates a misconfigured environment).
#[must_use]
pub fn default_persistent_db_path() -> Option<PathBuf> {
    Some(
        dirs::data_dir()?
            .join(APP_DIR_NAME)
            .join(PERSISTENT_DB_FILENAME),
    )
}

/// Resolve where the persistent database should live, applying the
/// CLI > env-var > platform-default precedence. Returns `None` only when
/// no source supplied a path (the platform default failed *and* nothing
/// was set explicitly), which the caller should treat as an error.
///
/// `cli_value` is the value of `--persistent-db` (or `--workspace` after
/// deprecation translation). When `Some`, takes precedence over both
/// the env var and the platform default.
#[must_use]
pub fn resolve_persistent_db_path(cli_value: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = cli_value {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os(ENV_PERSISTENT_DB) {
        return Some(PathBuf::from(path));
    }
    default_persistent_db_path()
}

/// Resolve the persistent database while retaining the winning source.
///
/// Callers validate mutually exclusive CLI flags before invoking this helper.
/// Selection then preserves the existing CLI > environment > platform-default
/// precedence, while distinguishing the preferred and deprecated CLI spellings.
#[must_use]
pub fn resolve_persistent_db_path_with_source(
    cli_value: Option<&str>,
    deprecated_alias: Option<&str>,
    disabled: bool,
) -> ResolvedPersistentDbPath {
    if disabled {
        return ResolvedPersistentDbPath {
            path: None,
            observed_path: None,
            source: PersistentDbPathSource::Disabled,
        };
    }
    if let Some(path) = cli_value {
        return resolved_persistent_path(PathBuf::from(path), PersistentDbPathSource::Cli);
    }
    if let Some(path) = deprecated_alias {
        return resolved_persistent_path(
            PathBuf::from(path),
            PersistentDbPathSource::DeprecatedAlias,
        );
    }
    if let Some(path) = std::env::var_os(ENV_PERSISTENT_DB) {
        return resolved_persistent_path(PathBuf::from(path), PersistentDbPathSource::Environment);
    }
    match default_persistent_db_path() {
        Some(path) => resolved_persistent_path(path, PersistentDbPathSource::PlatformDefault),
        None => ResolvedPersistentDbPath {
            path: None,
            observed_path: None,
            source: PersistentDbPathSource::PlatformDefault,
        },
    }
}

fn resolved_persistent_path(
    observed_path: PathBuf,
    source: PersistentDbPathSource,
) -> ResolvedPersistentDbPath {
    let path = Some(effective_persistent_db_path(observed_path.as_os_str()));
    ResolvedPersistentDbPath {
        path,
        observed_path: Some(observed_path),
        source,
    }
}

/// Apply the same string conversion and literal `~/` expansion used before an
/// [`crate::engine::Engine`] opens a persistent database.
pub(crate) fn effective_persistent_db_path(path: &OsStr) -> PathBuf {
    let path = path.to_string_lossy();
    let rest = if let Some(rest) = path.strip_prefix("~/") {
        Some(rest)
    } else if cfg!(windows) {
        path.strip_prefix("~\\")
    } else {
        None
    };
    let Some(rest) = rest else {
        return PathBuf::from(path.as_ref());
    };
    let Some(home) = persistent_home_dir() else {
        return PathBuf::from(path.as_ref());
    };
    let separator = std::path::MAIN_SEPARATOR;
    PathBuf::from(format!("{}{separator}{rest}", home.to_string_lossy()))
}

fn persistent_home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            if !profile.is_empty() {
                return Some(PathBuf::from(profile));
            }
        }
        let drive = std::env::var_os("HOMEDRIVE")?;
        let relative = std::env::var_os("HOMEPATH")?;
        let mut combined = PathBuf::from(drive);
        combined.push(PathBuf::from(relative));
        Some(combined)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-wide lock for env-var tests. `std::env::set_var` is
    /// `unsafe` in newer toolchains because it's not thread-safe; we
    /// serialize all env-touching tests to keep them sound.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env_lock<R>(f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f()
    }

    /// Sets an env var. Marked `unsafe` because [`std::env::set_var`] is
    /// `unsafe` in newer toolchains; callers hold `ENV_LOCK`.
    unsafe fn set_env(key: &str, value: &str) {
        // SAFETY: serialized by ENV_LOCK; matches std::env contract.
        unsafe { std::env::set_var(key, value) }
    }

    /// Removes an env var. Marked `unsafe` for the same reason as
    /// [`set_env`]; callers hold `ENV_LOCK`.
    unsafe fn remove_env(key: &str) {
        // SAFETY: serialized by ENV_LOCK; matches std::env contract.
        unsafe { std::env::remove_var(key) }
    }

    #[test]
    fn default_persistent_db_path_returns_some_on_supported_platforms() {
        // On macOS, Linux, and Windows the platform helpers always
        // resolve to a usable path. CI runs on these three; if this
        // fails on a new platform we want a loud signal.
        let p = default_persistent_db_path().expect("platform data_dir resolves");
        assert!(p.ends_with("hyperdb/workspace.hyper") || p.ends_with("hyperdb\\workspace.hyper"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_persistent_db_path_uses_app_support_on_macos() {
        let p = default_persistent_db_path().unwrap();
        let s = p.to_string_lossy();
        assert!(
            s.contains("Library/Application Support/hyperdb/"),
            "expected macOS Application Support path, got {s}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn default_persistent_db_path_uses_xdg_share_on_linux() {
        let p = default_persistent_db_path().unwrap();
        let s = p.to_string_lossy();
        assert!(
            s.contains(".local/share/hyperdb/") || s.contains("share/hyperdb/"),
            "expected XDG share path, got {s}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn default_persistent_db_path_uses_appdata_on_windows() {
        let p = default_persistent_db_path().unwrap();
        let s = p.to_string_lossy();
        assert!(
            s.contains("hyperdb"),
            "expected APPDATA path containing hyperdb, got {s}"
        );
    }

    #[test]
    fn resolve_persistent_db_path_cli_takes_precedence() {
        with_env_lock(|| {
            // SAFETY: serialized by ENV_LOCK.
            unsafe { set_env(ENV_PERSISTENT_DB, "/from/env.hyper") };
            let p = resolve_persistent_db_path(Some("/from/cli.hyper"))
                .expect("CLI path always resolves");
            assert_eq!(p, PathBuf::from("/from/cli.hyper"));
            // SAFETY: serialized by ENV_LOCK.
            unsafe { remove_env(ENV_PERSISTENT_DB) };
        });
    }

    #[test]
    fn resolve_persistent_db_path_env_used_when_no_cli() {
        with_env_lock(|| {
            // SAFETY: serialized by ENV_LOCK.
            unsafe { set_env(ENV_PERSISTENT_DB, "/from/env.hyper") };
            let p = resolve_persistent_db_path(None).expect("env path resolves");
            assert_eq!(p, PathBuf::from("/from/env.hyper"));
            // SAFETY: serialized by ENV_LOCK.
            unsafe { remove_env(ENV_PERSISTENT_DB) };
        });
    }

    #[test]
    fn resolve_persistent_db_path_falls_back_to_default() {
        with_env_lock(|| {
            // SAFETY: serialized by ENV_LOCK.
            unsafe { remove_env(ENV_PERSISTENT_DB) };
            let p = resolve_persistent_db_path(None).expect("default resolves");
            // Just check it's under hyperdb/ — exact location varies by
            // platform and is covered by the default-path tests above.
            assert!(p.to_string_lossy().contains("hyperdb"));
        });
    }

    #[test]
    fn nested_tilde_source_resolution_preserves_raw_input_for_one_runtime_expansion() {
        use std::io::Read as _;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CHILD_SENTINEL: &str = "HYPERDB_MCP_PATHS_NESTED_TILDE_CHILD";
        const CHILD_SENTINEL_VALUE: &str = "nested-tilde-source-child-v1";
        const TEST_NAME: &str =
            "paths::tests::nested_tilde_source_resolution_preserves_raw_input_for_one_runtime_expansion";

        if std::env::var(CHILD_SENTINEL).as_deref() == Ok(CHILD_SENTINEL_VALUE) {
            let legacy = resolve_persistent_db_path(Some("~/data.hyper"));
            assert_eq!(
                legacy,
                Some(PathBuf::from("~/data.hyper")),
                "legacy callers must continue receiving the raw winning path"
            );

            let source_aware =
                resolve_persistent_db_path_with_source(Some("~/data.hyper"), None, false);
            assert_eq!(source_aware.source, PersistentDbPathSource::Cli);
            assert_eq!(
                source_aware.observed_path,
                Some(PathBuf::from("~/data.hyper")),
                "source-aware resolution must retain the raw value main can pass to Engine"
            );
            assert_eq!(
                source_aware.path,
                Some(PathBuf::from("~/outer").join("data.hyper")),
                "diagnostics may report exactly one literal home-prefix expansion"
            );
            return;
        }

        let mut child =
            Command::new(std::env::current_exe().expect("locate current libtest binary"))
                .arg("--exact")
                .arg(TEST_NAME)
                .arg("--nocapture")
                .env(CHILD_SENTINEL, CHILD_SENTINEL_VALUE)
                .env("HOME", "~/outer")
                .env("USERPROFILE", "~/outer")
                .env_remove(ENV_PERSISTENT_DB)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn isolated nested-tilde libtest child");
        let mut child_stdout = child.stdout.take().expect("capture child stdout");
        let mut child_stderr = child.stderr.take().expect("capture child stderr");
        let stdout_reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            child_stdout
                .read_to_end(&mut output)
                .expect("read nested-tilde child stdout");
            output
        });
        let stderr_reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            child_stderr
                .read_to_end(&mut output)
                .expect("read nested-tilde child stderr");
            output
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        let completion = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let kill_result = child.kill();
                    let wait_result = child.wait();
                    break Err(format!(
                        "nested-tilde child exceeded ten-second watchdog; kill={kill_result:?}; wait={wait_result:?}"
                    ));
                }
                Err(error) => {
                    let kill_result = child.kill();
                    let wait_result = child.wait();
                    break Err(format!(
                        "could not poll nested-tilde child: {error}; kill={kill_result:?}; wait={wait_result:?}"
                    ));
                }
            }
        };
        let stdout = stdout_reader
            .join()
            .expect("nested-tilde stdout reader must not panic");
        let stderr = stderr_reader
            .join()
            .expect("nested-tilde stderr reader must not panic");
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);

        let status = completion.unwrap_or_else(|error| {
            panic!("{error}\nchild stdout:\n{stdout}\nchild stderr:\n{stderr}")
        });
        assert!(
            status.success(),
            "nested-tilde child failed with {status}\nchild stdout:\n{stdout}\nchild stderr:\n{stderr}"
        );
        assert!(
            stdout.contains("running 1 test") && stdout.contains(TEST_NAME),
            "nested-tilde child exact filter did not execute one named test:\n{stdout}"
        );
    }
}
