// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the async [`AsyncKvStore`] API.

mod common;

use common::{test_hyper_params, test_result_path};
use hyperdb_api::{AsyncConnection, CreateMode, Error, HyperProcess, Result};
use serde::{Deserialize, Serialize};

async fn fresh_async_conn(name: &str) -> Result<(HyperProcess, AsyncConnection)> {
    let db_path = test_result_path(name, "hyper")?;
    let params = test_hyper_params(name)?;
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?.to_string();
    let conn = AsyncConnection::connect(
        &endpoint,
        db_path.to_str().expect("path"),
        CreateMode::CreateAndReplace,
    )
    .await?;
    Ok((hyper, conn))
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Profile {
    name: String,
    level: u32,
}

#[tokio::test(flavor = "current_thread")]
async fn async_kv_full_surface() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_full").await?;
    let kv = conn.kv_store("cfg").await?;

    assert_eq!(kv.get("missing").await?, None);
    let _ = kv.set("k", "v1").await?;
    let _ = kv.set("k", "v2").await?;
    assert_eq!(kv.get("k").await?, Some("v2".to_string()));

    let p = Profile {
        name: "ada".into(),
        level: 7,
    };
    let _ = kv.set_as("p", &p).await?;
    assert_eq!(kv.get_as::<Profile>("p").await?, Some(p));
    assert!(matches!(
        kv.get_as::<Profile>("k").await,
        Err(Error::Serialization(_))
    ));

    let _ = kv.set_batch(&[("a", "1"), ("b", "2")]).await?;
    assert_eq!(kv.size().await?, 4);
    assert_eq!(kv.keys().await?, vec!["a", "b", "k", "p"]);
    assert!(kv.exists("a").await?);
    assert!(kv.delete("a").await?);
    assert!(!kv.delete("a").await?);

    assert_eq!(kv.pop().await?, Some(("b".to_string(), "2".to_string())));

    // After delete("a") + pop() removed "b", exactly "k" and "p" remain.
    let removed = kv.clear().await?;
    assert_eq!(removed, 2);
    assert_eq!(kv.size().await?, 0);
    // pop on an empty store yields None (mirrors the sync contract).
    assert_eq!(kv.pop().await?, None);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_list_stores_and_validation() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_list").await?;
    assert!(conn.kv_list_stores().await?.is_empty());
    let _ = conn.kv_store("alpha").await?.set("k", "1").await?;
    let _ = conn.kv_store("beta").await?.set("k", "2").await?;
    let mut stores = conn.kv_list_stores().await?;
    stores.sort();
    assert_eq!(stores, vec!["alpha", "beta"]);
    assert!(matches!(
        conn.kv_store("bad name").await,
        Err(Error::InvalidName(_))
    ));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_validates_key_on_every_entry_point() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_validate").await?;
    let kv = conn.kv_store("cfg").await?;
    // Every key-taking async method must reject an invalid key with
    // `InvalidName` before touching the wire (mirrors the sync contract).
    assert!(matches!(
        kv.set("bad key", "v").await,
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        kv.get("bad key").await,
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        kv.delete("bad key").await,
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        kv.exists("bad key").await,
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        kv.get_as::<Profile>("bad key").await,
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        kv.set_as("bad key", &3u32).await,
        Err(Error::InvalidName(_))
    ));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_store_isolation() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_isolation").await?;
    let a = conn.kv_store("alpha").await?;
    let b = conn.kv_store("beta").await?;
    // Same key in two stores resolves to each store's own value.
    let _ = a.set("k", "from_alpha").await?;
    let _ = b.set("k", "from_beta").await?;
    assert_eq!(a.get("k").await?, Some("from_alpha".to_string()));
    assert_eq!(b.get("k").await?, Some("from_beta".to_string()));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_set_reports_created_and_batch_outcome() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_outcome").await?;
    let kv = conn.kv_store("outcome").await?;
    assert!(kv.set("k", "v1").await?.created);
    assert!(!kv.set("k", "v2").await?.created);

    kv.set("a", "1").await?; // pre-existing
    let out = kv.set_batch(&[("a", "10"), ("b", "20")]).await?;
    assert_eq!(out.created, 1);
    assert_eq!(out.overwritten, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_byte_size_and_entries() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_sized").await?;
    let kv = conn.kv_store("sized").await?;
    assert_eq!(kv.byte_size().await?, 0, "empty store has 0 bytes");
    kv.set("a", "hello").await?; // 5 bytes
    kv.set("b", "worlds").await?; // 6 bytes
    assert_eq!(kv.byte_size().await?, 11, "sum of OCTET_LENGTH");
    assert_eq!(
        kv.entries().await?,
        vec![
            ("a".to_string(), "hello".to_string()),
            ("b".to_string(), "worlds".to_string()),
        ],
        "entries sorted by key with values"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_guard_size_and_entries() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_guard").await?;
    let kv = conn.kv_store("g").await?;
    assert!(kv.set_if_absent("k", "first").await?);
    assert!(!kv.set_if_absent("k", "second").await?);
    assert_eq!(kv.get("k").await?, Some("first".to_string()));

    assert_eq!(kv.byte_size().await?, 5); // "first"
    kv.set("z", "hello").await?; // 5 more
    assert_eq!(kv.byte_size().await?, 10);
    assert_eq!(
        kv.entries().await?,
        vec![
            ("k".to_string(), "first".to_string()),
            ("z".to_string(), "hello".to_string()),
        ]
    );

    let out = kv.set_batch_if_absent(&[("k", "x"), ("new", "n1")]).await?;
    assert_eq!(out.written, 1);
    assert_eq!(out.skipped, 1);
    Ok(())
}
