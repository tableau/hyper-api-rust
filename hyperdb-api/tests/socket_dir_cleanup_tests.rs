// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `HyperProcess::drop` must only remove a socket directory it created itself.

#![cfg(unix)]

use hyperdb_api::{HyperProcess, Parameters, TransportMode};

/// A caller-supplied `domain_socket_directory` whose basename happens to start
/// with `hyper-` (e.g. `~/hyper-data`) is user data, not scratch space. Drop
/// used to `remove_dir_all` it purely on the name prefix.
#[test]
fn user_supplied_socket_dir_survives_drop() {
    // Keep the path short: Unix socket paths are limited to ~104 bytes.
    let dir = std::path::PathBuf::from(format!("/tmp/hyper-keep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create user dir");
    let sentinel = dir.join("precious.txt");
    std::fs::write(&sentinel, b"do not delete").expect("write sentinel");

    let mut params = Parameters::new();
    params.set_transport_mode(TransportMode::Ipc);
    params.set_domain_socket_directory(&dir);
    {
        let hyper = HyperProcess::new(None, Some(&params)).expect("start hyperd over IPC");
        assert_eq!(hyper.socket_directory(), Some(dir.as_path()));
    }

    let survived = sentinel.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(survived, "Drop deleted a user-supplied socket directory");
}

/// The default temp socket directory is ours and must still be cleaned up.
#[test]
fn default_socket_dir_is_removed_on_drop() {
    let mut params = Parameters::new();
    params.set_transport_mode(TransportMode::Ipc);
    let dir = {
        let hyper = HyperProcess::new(None, Some(&params)).expect("start hyperd over IPC");
        let dir = hyper
            .socket_directory()
            .expect("ipc socket dir")
            .to_path_buf();
        assert!(dir.exists());
        dir
    };
    assert!(!dir.exists(), "default socket dir should be cleaned up");
}

/// `connection_endpoint_string` must be connectable even when the caller gave
/// a relative socket directory: a relative path would be taken for a TCP host
/// by `Connection::new`, so the directory is made absolute up front.
#[test]
fn relative_socket_directory_yields_connectable_endpoint_string() {
    let rel = std::path::PathBuf::from(format!("rs-{}", std::process::id()));
    std::fs::create_dir_all(&rel).expect("create relative dir");

    /// Removes the test's leftovers even when an assertion panics.
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            let _ = std::fs::remove_file("rel_socket.hyper");
        }
    }
    let _cleanup = Cleanup(rel.clone());

    let mut params = Parameters::new();
    params.set_transport_mode(TransportMode::Ipc);
    params.set_domain_socket_directory(&rel);
    let hyper = HyperProcess::new(None, Some(&params)).expect("start hyperd over IPC");
    let endpoint = hyper
        .connection_endpoint_string()
        .expect("IPC process has an endpoint");
    assert!(
        endpoint.starts_with('/'),
        "endpoint must be absolute so it is routed to the Unix socket: {endpoint}"
    );
    let conn = hyperdb_api::Connection::new(
        &hyper,
        "rel_socket.hyper",
        hyperdb_api::CreateMode::CreateAndReplace,
    )
    .expect("connect over the relative-dir socket");
    conn.execute_command("SELECT 1").expect("query");
}
