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
    kv.set("k", "v1").await?;
    kv.set("k", "v2").await?;
    assert_eq!(kv.get("k").await?, Some("v2".to_string()));

    let p = Profile {
        name: "ada".into(),
        level: 7,
    };
    kv.set_as("p", &p).await?;
    assert_eq!(kv.get_as::<Profile>("p").await?, Some(p));
    assert!(matches!(
        kv.get_as::<Profile>("k").await,
        Err(Error::Serialization(_))
    ));

    kv.set_batch(&[("a", "1"), ("b", "2")]).await?;
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
    conn.kv_store("alpha").await?.set("k", "1").await?;
    conn.kv_store("beta").await?.set("k", "2").await?;
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
    a.set("k", "from_alpha").await?;
    b.set("k", "from_beta").await?;
    assert_eq!(a.get("k").await?, Some("from_alpha".to_string()));
    assert_eq!(b.get("k").await?, Some("from_beta".to_string()));
    Ok(())
}
