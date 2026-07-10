// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for `Connection::kv_store_in` / `kv_list_stores_in`
//! (targeted KV location), sync and async.

mod common;

use common::{test_hyper_params, test_result_path};
use hyperdb_api::{escape_sql_path, AsyncConnection, Connection, CreateMode, HyperProcess, Result};

/// A KV store opened in an attached database is isolated from the primary DB,
/// round-trips set/get through the attached location, and is enumerated by the
/// location-aware store listing.
#[test]
fn kv_store_in_targets_attached_database() -> Result<()> {
    let params = test_hyper_params("kv_store_in_sync")?;
    let hyper = HyperProcess::new(None, Some(&params))?;

    // Primary (ephemeral-style) DB.
    let primary = test_result_path("kv_store_in_sync_primary", "hyper")?;
    let conn = Connection::new(&hyper, &primary, CreateMode::CreateAndReplace)?;
    let primary_stem = primary
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("primary stem");

    // Attach a second DB under alias "aux".
    let aux_path = test_result_path("kv_store_in_sync_aux", "hyper")?;
    let _ = std::fs::remove_file(&aux_path);
    let aux_str = aux_path.to_string_lossy();
    conn.execute_command(&format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        escape_sql_path(&aux_str)
    ))?;
    conn.execute_command(&format!(
        "ATTACH DATABASE {} AS \"aux\"",
        escape_sql_path(&aux_str)
    ))?;
    // ATTACH shifts schema_search_path; re-pin to the primary so the *default*
    // (unqualified) KV location keeps resolving to the primary DB. Mirrors
    // `hyperdb-mcp`'s engine, which pins search_path at attach time.
    conn.execute_command(&format!(
        "SET schema_search_path = '{}'",
        primary_stem.replace('\'', "''")
    ))?;

    // Open a KV store in the attached DB — pass the BARE alias; escaping is internal.
    let kv = conn.kv_store_in("aux", "settings")?;
    kv.set("theme", "dark")?;
    assert_eq!(kv.get("theme")?, Some("dark".to_string()));

    // The default-location store must NOT see the attached-DB value.
    let default_kv = conn.kv_store("settings")?;
    assert_eq!(default_kv.get("theme")?, None);

    // The location-aware listing sees the attached-DB store; the primary has none.
    assert_eq!(conn.kv_list_stores_in("aux")?, vec!["settings".to_string()]);
    assert!(conn.kv_list_stores()?.is_empty());

    Ok(())
}

/// Async twin of the above — proves the async `kv_store_in` / `kv_list_stores_in`
/// route to the attached DB and stay isolated from the primary.
#[tokio::test(flavor = "current_thread")]
async fn async_kv_store_in_targets_attached_database() -> Result<()> {
    let params = test_hyper_params("kv_store_in_async")?;
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?.to_string();

    let primary = test_result_path("kv_store_in_async_primary", "hyper")?;
    let conn = AsyncConnection::connect(
        &endpoint,
        primary.to_str().expect("primary path"),
        CreateMode::CreateAndReplace,
    )
    .await?;
    let primary_stem = primary
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("primary stem");

    let aux_path = test_result_path("kv_store_in_async_aux", "hyper")?;
    let _ = std::fs::remove_file(&aux_path);
    let aux_str = aux_path.to_string_lossy();
    conn.execute_command(&format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        escape_sql_path(&aux_str)
    ))
    .await?;
    conn.execute_command(&format!(
        "ATTACH DATABASE {} AS \"aux\"",
        escape_sql_path(&aux_str)
    ))
    .await?;
    conn.execute_command(&format!(
        "SET schema_search_path = '{}'",
        primary_stem.replace('\'', "''")
    ))
    .await?;

    let kv = conn.kv_store_in("aux", "settings").await?;
    kv.set("theme", "dark").await?;
    assert_eq!(kv.get("theme").await?, Some("dark".to_string()));
    assert_eq!(conn.kv_store("settings").await?.get("theme").await?, None);
    assert_eq!(
        conn.kv_list_stores_in("aux").await?,
        vec!["settings".to_string()]
    );
    assert!(conn.kv_list_stores().await?.is_empty());

    Ok(())
}
