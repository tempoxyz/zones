//! L1 state listener that tracks Tempo L1 storage for zone-side reads.
//!
//! The [`L1ChainNotificationListener`] receives full state diffs from the L1 node (in-process).
//! On each commit it proactively writes changed storage slots for tracked contracts into the
//! cache. On reorgs the cache is cleared and re-populated from the new chain segment.
//!
//! For lighter-weight anchor tracking (header-only), the unified
//! [`L1Subscriber`](crate::l1::L1Subscriber) handles anchor updates and reorg detection
//! as part of its block processing loop.

use crate::l1_state::cache::SharedL1StateCache;
use alloy_eips::NumHash;
use alloy_primitives::B256;
use futures::StreamExt;
use reth_primitives_traits::AlloyBlockHeader as _;
use reth_provider::{CanonStateNotification, CanonStateSubscriptions};
use tracing::{debug, error, info, warn};

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
