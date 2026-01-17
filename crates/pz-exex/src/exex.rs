//! Privacy Zone Execution Extension.
//!
//! Based on reth-exex-examples/rollup pattern with Signet-inspired improvements.
//! Uses in-memory ZoneState (CacheDB-based) instead of SQL.

use crate::{
    state::ZoneState,
    types::{
        BatchSubmitted, Deposit, DepositEnqueued, L1Cursor, PortalEvent, PortalEventKind, PzConfig,
    },
};
use alloy_sol_types::SolEvent;
use futures::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_tracing::tracing::{debug, info, warn};

/// Privacy Zone Execution Extension.
pub struct PrivacyZoneExEx<Node: FullNodeComponents> {
    ctx: ExExContext<Node>,
    state: ZoneState,
    config: PzConfig,
}

impl<Node: FullNodeComponents> PrivacyZoneExEx<Node> {
    /// Create a new Privacy Zone ExEx.
    pub fn new(ctx: ExExContext<Node>, config: PzConfig) -> Self {
        Self {
            ctx,
            state: ZoneState::new(),
            config,
        }
    }

    /// Start processing chain notifications.
    pub async fn start(mut self) -> eyre::Result<()> {
        info!(
            zone_id = self.config.zone_id,
            portal = %self.config.portal_address,
            "Starting Privacy Zone ExEx"
        );

        // Initialize zone state if needed
        self.init_zone_state()?;

        while let Some(notification) = self.ctx.notifications.try_next().await? {
            // Handle reverts first (Signet pattern)
            if let Some(reverted_chain) = notification.reverted_chain() {
                self.on_revert(&reverted_chain)?;
            }

            // Then handle commits
            if let Some(committed_chain) = notification.committed_chain() {
                self.on_commit(&committed_chain)?;
                self.ctx
                    .events
                    .send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
            }
        }

        Ok(())
    }

    /// Initialize zone state from genesis if not already set.
    fn init_zone_state(&mut self) -> eyre::Result<()> {
        if self.state.config().is_none() {
            info!(zone_id = self.config.zone_id, "Initializing zone state");
            self.state.set_config(self.config.clone());
        }
        Ok(())
    }

    /// Process a committed chain - extract events and process deposits.
    fn on_commit(
        &mut self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<()> {
        // Extract all portal events from the chain
        let events = self.extract_portal_events(chain);

        for event in events {
            let current_cursor = self.state.zone_state().cursor;

            // Skip events we've already processed (cursor-based dedup)
            if !event.cursor.is_after(&current_cursor) {
                continue;
            }

            match &event.kind {
                PortalEventKind::Deposit(deposit) => {
                    self.handle_deposit(&event.cursor, deposit)?;
                }
                PortalEventKind::BatchSubmitted {
                    batch_index,
                    new_state_root,
                    ..
                } => {
                    debug!(
                        zone_id = self.config.zone_id,
                        batch_index = batch_index,
                        state_root = %new_state_root,
                        "Batch submitted on L1"
                    );
                }
            }

            // Update cursor after processing
            self.state.zone_state_mut().cursor = event.cursor;
        }

        Ok(())
    }

    /// Handle a deposit event - queue it for later execution in block building.
    ///
    /// Deposits are queued as pending transactions. When the block builder runs,
    /// it will pull from this queue, execute the deposits (credit TIP-20 + run calldata),
    /// and include them in a zone block.
    fn handle_deposit(&mut self, cursor: &L1Cursor, deposit: &Deposit) -> eyre::Result<()> {
        // Get current deposits hash for chain hash computation
        let prev_deposits_hash = self.state.zone_state().deposits_hash;

        // Compute deposit hash for the chain
        let deposit_hash = deposit.hash(prev_deposits_hash);

        // Queue deposit as pending transaction
        self.state
            .queue_deposit(*cursor, deposit.clone(), deposit_hash);

        // Update deposits hash in zone state
        self.state.zone_state_mut().deposits_hash = deposit_hash;

        info!(
            zone_id = self.config.zone_id,
            sender = %deposit.sender,
            to = %deposit.to,
            amount = %deposit.amount,
            l1_block = cursor.block_number,
            log_index = cursor.log_index,
            has_calldata = !deposit.data.is_empty(),
            pending_txs = self.state.pending_txs().len(),
            "Queued deposit"
        );

        Ok(())
    }

    /// Handle a chain revert - undo deposits and state changes.
    ///
    /// NOTE: For now, this only removes pending deposits/exits after the revert point.
    /// Full state revert (undoing TIP-20 credits and calldata effects) would require
    /// either journaling or rebuilding state from scratch. For in-memory state,
    /// we'd need to track reverts properly or use reth's BundleState with reverts.
    fn on_revert(
        &mut self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<()> {
        // Extract events in reverse order for logging
        let mut events = self.extract_portal_events(chain);
        events.reverse();

        let current_cursor = self.state.zone_state().cursor;

        for event in events {
            // Only revert events we've actually processed
            if !current_cursor.is_after(&event.cursor) && current_cursor != event.cursor {
                continue;
            }

            match &event.kind {
                PortalEventKind::Deposit(deposit) => {
                    warn!(
                        zone_id = self.config.zone_id,
                        to = %deposit.to,
                        amount = %deposit.amount,
                        l1_block = event.cursor.block_number,
                        "Reverting deposit (state rollback not fully implemented)"
                    );
                }
                PortalEventKind::BatchSubmitted { batch_index, .. } => {
                    warn!(
                        zone_id = self.config.zone_id,
                        batch_index = batch_index,
                        "Reverting batch"
                    );
                }
            }
        }

        // Find the cursor to revert to (the first event in the reverted chain)
        if let Some((&_block_num, first_block)) = chain.blocks().iter().next() {
            use alloy_consensus::BlockHeader;
            let revert_to_block = first_block.header().number().saturating_sub(1);

            // Remove deposits after the revert point
            let revert_cursor = L1Cursor::new(revert_to_block, u64::MAX);
            self.state.remove_deposits_after(revert_cursor);

            // Update state cursor
            self.state.zone_state_mut().cursor = revert_cursor;
        }

        Ok(())
    }

    /// Extract portal events from a chain (Signet-inspired extraction pattern).
    fn extract_portal_events(
        &self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> Vec<PortalEvent> {
        use alloy_consensus::{BlockHeader as _, TxReceipt};

        let mut events = Vec::new();

        for (block, receipts) in chain.blocks_and_receipts() {
            let block_number = block.header().number();
            let mut log_index = 0u64;

            for receipt in receipts.iter() {
                for log in receipt.logs() {
                    // Only process logs from our portal
                    if log.address != self.config.portal_address {
                        log_index += 1;
                        continue;
                    }

                    let cursor = L1Cursor::new(block_number, log_index);

                    // Try to decode as deposit event
                    if let Ok(deposit_event) = DepositEnqueued::decode_raw_log(
                        log.topics().iter().copied(),
                        &log.data.data,
                    ) && deposit_event.zoneId == self.config.zone_id
                    {
                        let deposit = Deposit {
                            l1_block_hash: deposit_event.l1BlockHash,
                            l1_block_number: deposit_event.l1BlockNumber,
                            l1_timestamp: deposit_event.l1Timestamp,
                            sender: deposit_event.sender,
                            to: deposit_event.to,
                            amount: deposit_event.amount,
                            gas_limit: deposit_event.gasLimit,
                            data: deposit_event.data,
                        };
                        events.push(PortalEvent {
                            cursor,
                            kind: PortalEventKind::Deposit(deposit),
                        });
                    }

                    // Try to decode as batch submitted event
                    if let Ok(batch_event) =
                        BatchSubmitted::decode_raw_log(log.topics().iter().copied(), &log.data.data)
                        && batch_event.zoneId == self.config.zone_id
                    {
                        events.push(PortalEvent {
                            cursor,
                            kind: PortalEventKind::BatchSubmitted {
                                batch_index: batch_event.batchIndex,
                                new_state_root: batch_event.newStateRoot,
                                new_deposits_hash: batch_event.newDepositsHash,
                                exit_count: batch_event.exitCount,
                            },
                        });
                    }

                    log_index += 1;
                }
            }
        }

        events
    }
}

/// Install the Privacy Zone ExEx.
pub fn install_pz_exex<Node>(
    config: PzConfig,
) -> impl FnOnce(
    ExExContext<Node>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = eyre::Result<()>> + Send>>
where
    Node: FullNodeComponents + 'static,
{
    move |ctx| {
        Box::pin(async move {
            let exex = PrivacyZoneExEx::new(ctx, config);
            exex.start().await
        })
    }
}
