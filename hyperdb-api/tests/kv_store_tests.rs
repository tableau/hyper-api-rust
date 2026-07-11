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

use serde::{Deserialize, Serialize};

#[test]
fn set_then_get_and_overwrite() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    assert_eq!(kv.get("missing")?, None);
    kv.set("k", "v1")?;
    assert_eq!(kv.get("k")?, Some("v1".to_string()));
    kv.set("k", "v2")?; // upsert overwrite
    assert_eq!(kv.get("k")?, Some("v2".to_string()));
    Ok(())
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Profile {
    name: String,
    level: u32,
}

#[test]
fn set_as_get_as_round_trip() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    let p = Profile {
        name: "ada".into(),
        level: 7,
    };
    kv.set_as("profile", &p)?;
    assert_eq!(kv.get_as::<Profile>("profile")?, Some(p));
    assert_eq!(kv.get_as::<Profile>("absent")?, None);
    Ok(())
}

#[test]
fn get_as_malformed_json_is_serialization_error() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    kv.set("bad", "not json")?;
    let err = kv.get_as::<Profile>("bad").unwrap_err();
    assert!(matches!(err, Error::Serialization(_)));
    Ok(())
}

#[test]
fn validates_key_on_every_entry_point() {
    let tc = TestConnection::new().unwrap();
    let kv = tc.connection.kv_store("cfg").unwrap();
    // Every key-taking method must reject an invalid key with `InvalidName`
    // before touching the wire — validation is not gated behind `set`/`get`.
    assert!(matches!(kv.set("bad key", "v"), Err(Error::InvalidName(_))));
    assert!(matches!(kv.get("bad key"), Err(Error::InvalidName(_))));
    assert!(matches!(kv.delete("bad key"), Err(Error::InvalidName(_))));
    assert!(matches!(kv.exists("bad key"), Err(Error::InvalidName(_))));
    assert!(matches!(
        kv.get_as::<Profile>("bad key"),
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        kv.set_as("bad key", &3u32),
        Err(Error::InvalidName(_))
    ));
}

#[test]
fn delete_exists_size_keys_clear() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    kv.set("b", "2")?;
    kv.set("a", "1")?;
    kv.set("c", "3")?;

    assert_eq!(kv.size()?, 3);
    assert!(kv.exists("a")?);
    assert!(!kv.exists("z")?);
    assert_eq!(kv.keys()?, vec!["a", "b", "c"]); // ORDER BY key ASC

    assert!(kv.delete("b")?);
    assert!(!kv.delete("b")?); // already gone
    assert_eq!(kv.size()?, 2);

    let removed = kv.clear()?;
    assert_eq!(removed, 2);
    assert_eq!(kv.size()?, 0);
    Ok(())
}

#[test]
fn list_stores_and_isolation() -> Result<()> {
    let tc = TestConnection::new()?;
    // Empty before any store has keys.
    assert!(tc.connection.kv_list_stores()?.is_empty());

    let a = tc.connection.kv_store("alpha")?;
    let b = tc.connection.kv_store("beta")?;
    a.set("k", "from_alpha")?;
    b.set("k", "from_beta")?; // same key, different store

    assert_eq!(a.get("k")?, Some("from_alpha".to_string()));
    assert_eq!(b.get("k")?, Some("from_beta".to_string()));

    let mut stores = tc.connection.kv_list_stores()?;
    stores.sort();
    assert_eq!(stores, vec!["alpha", "beta"]);
    Ok(())
}

#[test]
fn pop_is_ordered_and_destructive() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("queue")?;
    kv.set("c", "3")?;
    kv.set("a", "1")?;
    kv.set("b", "2")?;

    assert_eq!(kv.pop()?, Some(("a".to_string(), "1".to_string())));
    assert_eq!(kv.pop()?, Some(("b".to_string(), "2".to_string())));
    assert_eq!(kv.pop()?, Some(("c".to_string(), "3".to_string())));
    assert_eq!(kv.pop()?, None); // empty
    assert_eq!(kv.size()?, 0);
    Ok(())
}

#[test]
fn set_batch_writes_all() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    kv.set_batch(&[("a", "1"), ("b", "2"), ("c", "3")])?;
    assert_eq!(kv.size()?, 3);
    assert_eq!(kv.get("b")?, Some("2".to_string()));
    // Batch upserts overwrite existing keys too.
    kv.set_batch(&[("b", "20"), ("d", "4")])?;
    assert_eq!(kv.get("b")?, Some("20".to_string()));
    assert_eq!(kv.size()?, 4);
    Ok(())
}

#[test]
fn set_batch_rejects_invalid_key_before_writing() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    let err = kv.set_batch(&[("ok", "1"), ("bad key", "2")]).unwrap_err();
    assert!(matches!(err, Error::InvalidName(_)));
    // Nothing was written because validation happens before the transaction.
    assert_eq!(kv.size()?, 0);
    Ok(())
}

#[test]
fn set_reports_created_then_overwritten() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("outcome")?;
    let first = kv.set("k", "v1")?;
    assert!(
        first.created,
        "first write of a key must report created=true"
    );
    let second = kv.set("k", "v2")?;
    assert!(!second.created, "overwrite must report created=false");
    assert_eq!(kv.get("k")?, Some("v2".to_string()));
    Ok(())
}

#[test]
fn set_batch_reports_created_and_overwritten() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("batch_outcome")?;
    kv.set("a", "1")?; // pre-existing → will be overwritten
    let out = kv.set_batch(&[("a", "10"), ("b", "20"), ("c", "30")])?;
    assert_eq!(out.created, 2, "b and c are new");
    assert_eq!(out.overwritten, 1, "a existed");
    assert_eq!(kv.get("a")?, Some("10".to_string()));
    Ok(())
}

#[test]
fn set_if_absent_guards_existing_key() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("guard")?;
    assert!(
        kv.set_if_absent("k", "first")?,
        "absent key must be written"
    );
    assert!(
        !kv.set_if_absent("k", "second")?,
        "present key must be skipped"
    );
    assert_eq!(
        kv.get("k")?,
        Some("first".to_string()),
        "value must be unchanged"
    );
    Ok(())
}

#[test]
fn byte_size_and_entries() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("sized")?;
    assert_eq!(kv.byte_size()?, 0, "empty store has 0 bytes");
    kv.set("a", "hello")?; // 5 bytes
    kv.set("b", "worlds")?; // 6 bytes
    assert_eq!(kv.byte_size()?, 11, "sum of OCTET_LENGTH");
    assert_eq!(
        kv.entries()?,
        vec![
            ("a".to_string(), "hello".to_string()),
            ("b".to_string(), "worlds".to_string()),
        ],
        "entries sorted by key with values"
    );
    Ok(())
}

#[test]
fn set_batch_if_absent_skips_existing() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("batch_guard")?;
    kv.set("a", "orig")?; // pre-existing → must be skipped
    let out = kv.set_batch_if_absent(&[("a", "new"), ("b", "b1"), ("c", "c1")])?;
    assert_eq!(out.written, 2, "b and c are new");
    assert_eq!(out.skipped, 1, "a existed");
    assert_eq!(
        kv.get("a")?,
        Some("orig".to_string()),
        "existing value untouched"
    );
    assert_eq!(kv.get("b")?, Some("b1".to_string()));
    Ok(())
}
