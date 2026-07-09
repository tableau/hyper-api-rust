// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the sync [`KvStore`] API.

mod common;

use common::TestConnection;
use hyperdb_api::{Error, Result};

#[test]
fn open_store_creates_backing_table() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    assert_eq!(kv.name(), "cfg");
    // Backing table exists and is initially empty for this store. Checked via a
    // direct COUNT (not `size()`, which arrives in Task 5) so Task 3 is
    // self-contained and compiles on its own.
    let count = tc
        .connection
        .execute_query("SELECT COUNT(*) FROM _hyperdb_kv_store WHERE store_name = 'cfg'")?
        .scalar::<i64>()?;
    assert_eq!(count, Some(0));
    Ok(())
}

#[test]
fn rejects_invalid_store_name() {
    let tc = TestConnection::new().unwrap();
    let err = tc.connection.kv_store("bad name").unwrap_err();
    assert!(matches!(err, Error::InvalidName(_)));
}

/// Documents the engine's duplicate-row behavior on the (PK-less) backing table.
/// The KV upsert guarantees single-row-per-key application-side; this test
/// records what the pinned `hyperd` does with a raw duplicate `INSERT` so
/// expectations stay honest and prove why the app-side upsert is required.
///
/// Empirically (2026-07-08) the table has NO `PRIMARY KEY` (Hyper rejects one:
/// `0A000: Index support is disabled`), so a raw duplicate insert is ACCEPTED —
/// which is exactly why `set` must use the conditional-INSERT idiom, not a bare
/// `INSERT`.
#[test]
fn documents_duplicate_insert_behavior() -> Result<()> {
    let tc = TestConnection::new()?;
    let _ = tc.connection.kv_store("dup_probe")?; // ensure table exists
    tc.connection.execute_command(
        "INSERT INTO _hyperdb_kv_store (store_name, key, value) VALUES ('dup_probe', 'k', 'v1')",
    )?;
    let dup = tc.connection.execute_command(
        "INSERT INTO _hyperdb_kv_store (store_name, key, value) VALUES ('dup_probe', 'k', 'v2')",
    );
    match dup {
        Err(e) => eprintln!("duplicate raw INSERT rejected -> {e}"),
        Ok(n) => eprintln!("duplicate raw INSERT accepted ({n} row); app-side upsert required"),
    }
    Ok(())
}
