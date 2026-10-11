// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Resource-update subscription registry backing the server-side hooks
//! for `resources/subscribe`, `resources/unsubscribe`, and the two MCP
//! notifications the server emits after workspace mutations.
//!
//! The registry keeps, per URI, a `Vec<Peer<RoleServer>>` of currently
//! subscribed clients. When a tool call changes workspace state the server
//! looks up every URI that would be affected (workspace, table list,
//! per-table schema/sample/csv-sample, per-saved-query result) and calls
//! [`SubscriptionRegistry::notify_updated`], which spawns a background
//! task per subscriber to send the notification.
//!
//! # Tradeoffs in this implementation
//!
//! `rmcp::Peer` is `Clone` but not `Eq`, and its internal `mpsc::Sender`
//! is not exposed for comparison. Rather than reach into private fields
//! we accept three pragmatic constraints:
//!
//! 1. Subscribing twice for the same URI from the same session stores two
//!    entries. Subsequent notifications fire twice. The MCP protocol
//!    defines no "already subscribed" error, so this is spec-compliant;
//!    well-behaved clients subscribe once per URI per session anyway.
//! 2. Unsubscribing clears *all* entries for the given URI. In multi-client
//!    setups this would be surprising, but for the stdio transport
//!    typically used with `HyperDB` each process serves at most a handful of
//!    clients and cross-client URI reuse is rare.
//! 3. Notify failures (client disconnected) are logged but not pruned.
//!    Dead peers accumulate until the MCP session tears the registry
//!    down. Notify is a bounded per-tool-call cost and sessions are
//!    short-lived, so the accumulation is harmless.

use rmcp::RoleServer;
use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::service::Peer;
use std::collections::HashMap;
use std::sync::Mutex;

/// Per-URI registry of subscribed peers with broadcast helpers.
///
/// Share it via `Arc` (typically `Arc<SubscriptionRegistry>` on the server).
/// Methods take `&self` because the internal [`Mutex`] provides interior
/// mutability.
#[derive(Debug, Default)]
pub struct SubscriptionRegistry {
    inner: Mutex<HashMap<String, Vec<Peer<RoleServer>>>>,
}

impl SubscriptionRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new subscription. See the module-level docs for the
    /// duplicate-subscribe behaviour: two calls with the same peer land
    /// two entries in the list.
    pub fn subscribe(&self, uri: &str, peer: Peer<RoleServer>) {
        let mut guard = self.lock();
        guard.entry(uri.to_string()).or_default().push(peer);
    }

    /// Remove all subscriptions for `uri`. Returns the number of entries
    /// that were removed.
    ///
    /// `_peer` is accepted for API symmetry with the MCP protocol
    /// (the incoming unsubscribe request carries no peer identity beyond
    /// "the session issuing the call"), but isn't used for matching.
    pub fn unsubscribe(&self, uri: &str, _peer: &Peer<RoleServer>) -> usize {
        let mut guard = self.lock();
        guard.remove(uri).map_or(0, |v| v.len())
    }

    /// Return a snapshot of all peers subscribed to `uri`. Useful for
    /// tests and for the notify helpers below, which want to release the
    /// mutex before doing any async work.
    pub fn subscribers_for(&self, uri: &str) -> Vec<Peer<RoleServer>> {
        let guard = self.lock();
        guard.get(uri).cloned().unwrap_or_default()
    }

    /// Return the set of URIs that currently have at least one subscriber.
    pub fn subscribed_uris(&self) -> Vec<String> {
        let guard = self.lock();
        guard.keys().cloned().collect()
    }

    /// Drop all subscriptions for every URI.
    pub fn clear(&self) {
        let mut guard = self.lock();
        guard.clear();
    }

    /// Fire a `notifications/resources/updated` for `uri` to every current
    /// subscriber. Sends happen in detached tokio tasks so the caller
    /// doesn't wait on the channel.
    ///
    /// If a send fails (typically because the peer disconnected) we log
    /// the error at `debug` level but do not prune the dead peer — see
    /// the module-level "Tradeoffs" note for why. Over very long-lived
    /// servers this can accumulate stale entries in the registry.
    ///
    /// No-op when the caller is outside a tokio runtime or the registry
    /// has no subscribers for `uri`.
    pub fn notify_updated(&self, uri: &str) {
        let peers = self.subscribers_for(uri);
        if peers.is_empty() {
            return;
        }
        for peer in peers {
            let uri = uri.to_string();
            // `tokio::spawn` is only valid inside a runtime; we're always
            // called from the rmcp server which runs on tokio. The `try_`
            // variant avoids panicking if someone misuses the registry
            // outside a runtime (e.g. in a synchronous unit test).
            let Ok(handle) = tokio::runtime::Handle::try_current() else {
                return;
            };
            let params = ResourceUpdatedNotificationParam::new(uri.clone());
            handle.spawn(async move {
                if let Err(e) = peer.notify_resource_updated(params).await {
                    tracing::debug!(uri = %uri, error = ?e, "resource update notify failed");
                }
            });
        }
    }

    /// Fire a `notifications/resources/list_changed` broadcast to every
    /// subscribed peer (across all URIs). Called when tables are added or
    /// dropped, or saved queries created / deleted.
    ///
    /// A peer subscribed to N URIs receives N notifications; clients treat
    /// `list_changed` idempotently.
    pub fn notify_list_changed(&self) {
        // Collect one peer per Vec entry. Peers cannot be compared, so a
        // multi-URI subscriber receives one notification per entry;
        // list_changed handlers on the client side are idempotent (they
        // trigger a resources/list refresh).
        let peers: Vec<Peer<RoleServer>> = {
            let guard = self.lock();
            guard.values().flat_map(|v| v.iter().cloned()).collect()
        };
        if peers.is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for peer in peers {
            handle.spawn(async move {
                if let Err(e) = peer.notify_resource_list_changed().await {
                    tracing::debug!(error = ?e, "resource list changed notify failed");
                }
            });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Peer<RoleServer>>>> {
        // Poisoning would indicate a panic inside another notify call;
        // falling back to the inner state still lets us serve the next
        // request rather than cascading the failure.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The full set of resource URIs that are affected when a specific table
/// is written to. Used by mutation-side helpers in `server.rs` to fire
/// targeted updates without sprinkling URI strings across every tool.
#[must_use]
pub fn uris_for_table_change(table: &str) -> Vec<String> {
    vec![
        "hyper://workspace".into(),
        "hyper://tables".into(),
        "hyper://readme".into(),
        format!("hyper://tables/{table}/schema"),
        format!("hyper://tables/{table}/sample"),
        format!("hyper://tables/{table}/csv-sample"),
    ]
}

/// The URIs affected by a workspace-wide change that doesn't name any
/// single table (e.g. watcher activity touching multiple files, or a
/// saved-query list mutation). `hyper://tables` reflects the updated
/// catalog, `hyper://readme` contains the summary that cross-references
/// everything else.
#[must_use]
pub fn uris_for_workspace_change() -> Vec<&'static str> {
    vec!["hyper://workspace", "hyper://tables", "hyper://readme"]
}
