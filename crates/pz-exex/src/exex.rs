//! Privacy Zone Execution Extension.
//!
//! Based on reth-exex-examples/rollup pattern with Signet-inspired improvements.
//! Spawns a block builder task that builds zone blocks every 250ms.

use crate::{
    builder::{BlockReceiver, SharedZoneState, ZoneBlock, ZoneBlockBuilder},
    types::{
        BatchSubmitted, Deposit, DepositEnqueued, L1Cursor, PortalEvent, PortalEventKind, PzConfig,
    },
};
use alloy_sol_types::SolEvent;
use futures::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent};
use reth_node_api::{FullNodeComponents, NodeTypes};

use reth_tracing::tracing::{debug, error, info, warn};
use std::sync::Arc;

/// Privacy Zone Execution Extension.
pub struct PrivacyZoneExEx<Node: FullNodeComponents> {
    ctx: ExExContext<Node>,
    state: Arc<SharedZoneState>,
    config: PzConfig,
    block_rx: BlockReceiver,
}

impl<Node: FullNodeComponents> PrivacyZoneExEx<Node> {
    /// Create a new Privacy Zone ExEx and spawn the block builder task.
    pub fn new(ctx: ExExContext<Node>, config: PzConfig) -> Self {
        // Create shared state
        let state = Arc::new(SharedZoneState::new());

        // Create block builder
        let (builder, block_rx) = ZoneBlockBuilder::new(config.clone(), Arc::clone(&state));

        // Spawn block builder as a critical task
        ctx.components.task_executor().spawn_critical(
            "pz-block-builder",
            Box::pin(async move {
                if let Err(err) = builder.await {
                    error!(%err, "Zone block builder failed");
                }
            }),
        );

        // TODO: batcher, channel is passed to batcher struct which posts to l1

        info!(zone_id = config.zone_id, "Spawned zone block builder task");

        Self {
            ctx,
            state,
            config,
            block_rx,
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

        loop {
            tokio::select! {
                // Handle L1 chain notifications
                notification = self.ctx.notifications.try_next() => {
                    match notification? {
                        Some(notification) => {
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
                        None => {
                            // Notification stream ended
                            break;
                        }
                    }
                }

                // Handle built zone blocks
                Some(block) = self.block_rx.recv() => {
                    self.on_zone_block_built(block)?;
                }
            }
        }

        Ok(())
    }

    /// Handle a newly built zone block.
    fn on_zone_block_built(&mut self, block: ZoneBlock) -> eyre::Result<()> {
        info!(
            zone_id = self.config.zone_id,
            block_number = block.number,
            block_hash = %block.hash,
            tx_count = block.tx_count,
            deposit_count = block.deposit_count,
            gas_used = block.gas_used,
            "Zone block built"
        );

        // TODO: persist block to zone database
        // TODO: post commitment to L1 (batch when ready)

        Ok(())
    }

    /// Initialize zone state from genesis if not already set.
    fn init_zone_state(&mut self) -> eyre::Result<()> {
        let mut state = self.state.lock();
        if state.config().is_none() {
            info!(zone_id = self.config.zone_id, "Initializing zone state");
            state.set_config(self.config.clone());
        }
        Ok(())
    }

    /// Process a committed chain - extract events and queue deposits.
    fn on_commit(
        &mut self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<()> {
        // Extract all portal events from the chain
        let events = self.extract_portal_events(chain);

        let mut state = self.state.lock();

        for event in events {
            let current_cursor = state.zone_state().cursor;

            // Skip events we've already processed (cursor-based dedup)
            if !event.cursor.is_after(&current_cursor) {
                continue;
            }

            match &event.kind {
                PortalEventKind::Deposit(deposit) => {
                    // Get current deposits hash for chain hash computation
                    let prev_deposits_hash = state.zone_state().deposits_hash;

                    // Compute deposit hash for the chain
                    let deposit_hash = deposit.hash(prev_deposits_hash);

                    // Queue deposit as pending transaction
                    state.queue_deposit(event.cursor, deposit.clone(), deposit_hash);

                    // Update deposits hash in zone state
                    state.zone_state_mut().deposits_hash = deposit_hash;

                    info!(
                        zone_id = self.config.zone_id,
                        sender = %deposit.sender,
                        to = %deposit.to,
                        amount = %deposit.amount,
                        l1_block = event.cursor.block_number,
                        log_index = event.cursor.log_index,
                        has_calldata = !deposit.data.is_empty(),
                        pending_txs = state.pending_txs().len(),
                        "Queued deposit"
                    );
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
            state.zone_state_mut().cursor = event.cursor;
        }

        Ok(())
    }

    /// Handle a chain revert - remove pending deposits after revert point.
    fn on_revert(
        &mut self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<()> {
        // Extract events in reverse order for logging
        let mut events = self.extract_portal_events(chain);
        events.reverse();

        let mut state = self.state.lock();
        let current_cursor = state.zone_state().cursor;

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
            state.remove_deposits_after(revert_cursor);

            // Update state cursor
            state.zone_state_mut().cursor = revert_cursor;
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
