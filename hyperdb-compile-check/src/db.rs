// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shared Hyper instance for compile-time validation.
//!
//! One `CompileTimeDb` is shared across all macro invocations in a single
//! crate compilation (rustc spawns one proc-macro host process per crate).
//! The instance is lazily initialized on first use via `get_or_init()` and
//! dropped when the host process exits.

use parking_lot::Mutex;

/// A live connection to an in-process Hyper instance used for SQL dry-runs.
#[derive(Debug)]
pub struct CompileTimeDb {
    _process: hyperdb_api::HyperProcess,
    pub(crate) conn: hyperdb_api::Connection,
}

// `HyperProcess` is the subprocess manager; a single instance can produce many
// `Connection`s. `Connection` is one TCP session — it is NOT designed for
// concurrent access from multiple threads (internal mutable TCP + protocol state).
//
// Neither type is `Send`/`Sync` in `hyperdb-api`'s public API. We implement
// both here because `OnceLock<T>` requires `T: Send + Sync`. The safety
// invariant is maintained entirely by the `parking_lot::Mutex` wrapper:
// the `Mutex` ensures only one thread holds a reference to the inner
// `CompileTimeDb` at any time, so `Connection` is never accessed concurrently.
//
// REVISIT: if `HyperProcess`/`Connection` are made `Send` upstream, remove
// these impls and let the compiler derive them.
//
// # Why `parking_lot::Mutex` instead of `std::sync::Mutex`
//
// Proc-macros routinely call `panic!` to emit a `compile_error!`. A
// `std::sync::Mutex` poisons on the first panic, causing every subsequent
// macro invocation in the same crate to receive `PoisonError` regardless of
// whether they have anything to do with the failing site. `parking_lot::Mutex`
// never poisons — lock acquisition always succeeds after the panicking thread
// releases the lock, so a bad `query_as!` site doesn't cascade.

// SAFETY: `OnceLock` requires `Send`; safe because the `Mutex` guarantees
// exclusive access — `CompileTimeDb` is never touched without holding the lock.
unsafe impl Send for CompileTimeDb {}
// SAFETY: `OnceLock` requires `Sync`; safe for the same reason as `Send` above.
unsafe impl Sync for CompileTimeDb {}

/// Global storage: initialized at most once per proc-macro host process.
///
/// We use `std::sync::OnceLock` (stable since 1.70) rather than a raw
/// `static mut` + `Once` pair to avoid the `static_mut_refs` UB concern in
/// Rust 2024 edition. `OnceLock` provides the same "write-once, read-many"
/// guarantee without unsafe code in the accessor.
static DB_STORAGE: std::sync::OnceLock<Mutex<CompileTimeDb>> = std::sync::OnceLock::new();

/// Returns a reference to the global `Mutex<CompileTimeDb>`, initializing it
/// on the first call.
///
/// # Panics
///
/// Panics if Hyper fails to start (e.g. `HYPERD_PATH` is invalid or the
/// binary is absent). The error is surfaced as a `compile_error!` by the
/// calling macro.
pub fn get_or_init() -> &'static Mutex<CompileTimeDb> {
    DB_STORAGE.get_or_init(|| {
        Mutex::new(CompileTimeDb::new().expect(
            "hyperdb-compile-check: failed to start embedded Hyper instance; \
                 check HYPERD_PATH or ensure .hyperd/current/hyperd is present",
        ))
    })
}

impl CompileTimeDb {
    fn new() -> hyperdb_api::Result<Self> {
        use hyperdb_api::{Connection, CreateMode, HyperProcess, Parameters};

        // Emit Hyper logs to a temp dir to keep build output clean.
        let log_dir = tempfile::tempdir().map_err(|e| {
            hyperdb_api::Error::Config(format!("compile-check: tempdir failed: {e}"))
        })?;
        let log_path = log_dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| log_dir.path().to_path_buf());

        let mut params = Parameters::new();
        params.set("log_dir", log_path.to_string_lossy().to_string());

        // `None` → auto-discover via HYPERD_PATH env or `.hyperd/current`.
        let process = HyperProcess::new(None, Some(&params))?;

        // In-memory validation database; each dry-run seeds required tables
        // on demand (lazy seeding via 42P01 SQLSTATE — see `validate.rs`).
        let db_path = log_dir.path().join("compile_check.hyper");
        let conn = Connection::new(&process, &db_path, CreateMode::CreateAndReplace)?;

        // Keep `log_dir` alive as long as the process — drop it with the struct.
        // We leak the TempDir intentionally: `CompileTimeDb` is `'static` (stored
        // in a static); the OS will clean up the temp dir on process exit.
        std::mem::forget(log_dir);

        Ok(Self {
            _process: process,
            conn,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn smoke_two_calls_reuse_instance() {
        let ptr1 = std::ptr::from_ref(get_or_init());
        let ptr2 = std::ptr::from_ref(get_or_init());
        assert_eq!(
            ptr1, ptr2,
            "get_or_init must return the same static instance"
        );
    }
}
