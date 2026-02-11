//! L1 state listener that tracks Tempo L1 storage for zone-side reads.
//!
//! Supports two subscription modes:
//!
//! - **Block headers** (`subscribe_blocks`) — lightweight; only updates the cache anchor and
//!   detects reorgs. Storage values are populated lazily via [`L1StateProvider`] RPC fallback.
//!
//! - **Chain notifications** (`CanonStateSubscriptions`) — receives full state diffs from the L1
//!   node (in-process). On each commit the listener proactively writes changed storage slots for
//!   tracked contracts into the cache. On reorgs the cache is cleared and re-populated from the
//!   new chain segment.
//!
//! [`L1StateProvider`]: super::provider::L1StateProvider

use crate::l1_state::cache::SharedL1StateCache;
use alloy_eips::NumHash;
use alloy_primitives::B256;
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use futures::StreamExt;
use reth_primitives_traits::AlloyBlockHeader as _;
use reth_provider::{CanonStateNotification, CanonStateSubscriptions};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Configuration for the L1 state listener.
#[derive(Debug, Clone)]
pub struct L1StateListenerConfig {
    /// WebSocket URL of the L1 node (used for header-only mode).
    pub l1_ws_url: String,
    /// Fallback poll interval if the WebSocket subscription drops.
    pub poll_interval: Duration,
}

impl Default for L1StateListenerConfig {
    fn default() -> Self {
        Self {
            l1_ws_url: "ws://localhost:8546".to_string(),
            poll_interval: Duration::from_secs(12),
        }
    }
}

/// Listener that subscribes to L1 block headers and keeps the state cache anchor current.
///
/// In header-only mode, storage values are populated lazily via `L1StateProvider` RPC fallback.
#[derive(Clone)]
pub struct L1StateListener {
    config: L1StateListenerConfig,
    cache: SharedL1StateCache,
}

impl L1StateListener {
    pub fn new(config: L1StateListenerConfig, cache: SharedL1StateCache) -> Self {
        Self { config, cache }
    }

    /// Connect to the L1 node via WebSocket and subscribe to new block headers.
    ///
    /// On each new block the cache anchor is updated. If the new block's parent hash does not
    /// match the current anchor (indicating a reorg), the cache is cleared first.
    pub async fn start(self) -> eyre::Result<()> {
        info!(url = %self.config.l1_ws_url, "Connecting to L1 for state listener");

        let ws = WsConnect::new(&self.config.l1_ws_url);
        let provider = ProviderBuilder::new().connect_ws(ws).await?;

        info!("Connected to L1 node, subscribing to block headers");

        let sub = provider.subscribe_blocks().await?;
        let mut stream = sub.into_stream();

        info!("Subscribed to L1 block headers");

        while let Some(header) = stream.next().await {
            let block_hash = header.hash;
            let block_number = header.number;
            let parent_hash = header.parent_hash;

            let mut cache = self.cache.write();
            let anchor = cache.anchor();

            if anchor.hash != B256::ZERO && parent_hash != anchor.hash {
                warn!(
                    old_anchor = %anchor.hash,
                    new_parent = %parent_hash,
                    block_number,
                    "Reorg detected, clearing L1 state cache"
                );
                cache.clear();
            }

            cache.update_anchor(NumHash {
                number: block_number,
                hash: block_hash,
            });
            debug!(block_hash = %block_hash, block_number, "Updated L1 state cache anchor");
        }

        warn!("L1 block header subscription stream ended");
        Ok(())
    }
}

/// Listener that consumes in-process `CanonStateSubscriptions` for full state-diff streaming.
///
/// Each [`CanonStateNotification::Commit`] proactively updates the cache with storage changes
/// for tracked contracts. [`CanonStateNotification::Reorg`] clears the cache and re-applies
/// the replacement segment.
pub struct L1ChainNotificationListener<C> {
    canon_state: C,
    cache: SharedL1StateCache,
}

impl<C> L1ChainNotificationListener<C>
where
    C: CanonStateSubscriptions + 'static,
{
    pub fn new(canon_state: C, cache: SharedL1StateCache) -> Self {
        Self { canon_state, cache }
    }

    /// Run the listener. Consumes the canonical state stream and applies storage diffs to the
    /// cache. This method only returns if the stream is closed.
    pub async fn run(self) -> eyre::Result<()> {
        let mut stream = self.canon_state.canonical_state_stream();

        info!("L1 chain notification listener started");

        while let Some(notification) = stream.next().await {
            match &notification {
                CanonStateNotification::Commit { new } => {
                    let tip = new.tip();
                    self.apply_state_diffs(new.execution_outcome(), tip.number());
                    let mut cache = self.cache.write();
                    cache.update_anchor(NumHash {
                        number: tip.number(),
                        hash: tip.hash(),
                    });
                    debug!(
                        block_hash = %tip.hash(),
                        block_number = tip.number(),
                        "L1 cache updated from chain notification (commit)"
                    );
                }
                CanonStateNotification::Reorg { old, new } => {
                    warn!(
                        reverted_blocks = old.len(),
                        new_blocks = new.len(),
                        "L1 reorg detected via chain notification, clearing cache"
                    );
                    {
                        let mut cache = self.cache.write();
                        cache.clear();
                    }
                    let tip = new.tip();
                    self.apply_state_diffs(new.execution_outcome(), tip.number());
                    let mut cache = self.cache.write();
                    cache.update_anchor(NumHash {
                        number: tip.number(),
                        hash: tip.hash(),
                    });
                }
            }
        }

        warn!("L1 chain notification stream ended");
        Ok(())
    }

    /// Extract storage changes from an `ExecutionOutcome` for tracked contracts and write them
    /// into the cache at the given block number.
    fn apply_state_diffs<R>(
        &self,
        execution_outcome: &reth_provider::ExecutionOutcome<R>,
        block_number: u64,
    ) {
        let mut cache = self.cache.write();
        let mut slots_updated: u64 = 0;

        for (address, bundle_account) in execution_outcome.bundle_accounts_iter() {
            if !cache.is_tracked(&address) {
                continue;
            }

            for (slot_key, slot) in &bundle_account.storage {
                let key = B256::from(slot_key.to_be_bytes::<32>());
                let value = B256::from(slot.present_value.to_be_bytes::<32>());
                cache.set(address, key, block_number, value);
                slots_updated += 1;
            }
        }

        if slots_updated > 0 {
            debug!(
                slots_updated,
                block_number, "Applied L1 state diffs to cache"
            );
        }
    }
}

/// Spawn the header-only L1 state listener as a critical background task with automatic
/// reconnection.
pub fn spawn_l1_state_listener(
    config: L1StateListenerConfig,
    cache: SharedL1StateCache,
    task_executor: impl reth_ethereum::tasks::TaskSpawner,
) {
    let listener = L1StateListener::new(config, cache);

    task_executor.spawn_critical(
        "l1-state-listener",
        Box::pin(async move {
            loop {
                if let Err(e) = listener.clone().start().await {
                    error!(error = %e, "L1 state listener failed, reconnecting in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }),
    );
}

/// Spawn the chain-notification L1 state listener as a critical background task.
///
/// This variant receives full state diffs from the L1 node (in-process) and proactively
/// populates the cache for tracked contracts.
pub fn spawn_l1_chain_notification_listener<C>(
    canon_state: C,
    cache: SharedL1StateCache,
    task_executor: impl reth_ethereum::tasks::TaskSpawner,
) where
    C: CanonStateSubscriptions + 'static,
{
    task_executor.spawn_critical(
        "l1-chain-notification-listener",
        Box::pin(async move {
            let listener = L1ChainNotificationListener::new(canon_state, cache);
            if let Err(e) = listener.run().await {
                error!(error = %e, "L1 chain notification listener failed");
            }
        }),
    );
}
