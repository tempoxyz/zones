//! Privacy Zone Execution Extension.
//!
//! Based on reth-exex-examples/rollup pattern with Signet-inspired improvements.

use crate::{
    db::Database,
    types::{
        BatchSubmitted, Deposit, DepositEnqueued, L1Cursor, PortalEvent, PortalEventKind, PzConfig,
        PzState,
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
    db: Database,
    config: PzConfig,
}

impl<Node: FullNodeComponents> PrivacyZoneExEx<Node> {
    /// Create a new Privacy Zone ExEx.
    pub fn new(ctx: ExExContext<Node>, db: Database, config: PzConfig) -> Self {
        Self { ctx, db, config }
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
    fn init_zone_state(&self) -> eyre::Result<()> {
        if self.db.get_zone_config()?.is_none() {
            info!(zone_id = self.config.zone_id, "Initializing zone state");
            self.db.set_zone_config(&self.config)?;
            self.db.set_zone_state(&PzState::default())?;
        }
        Ok(())
    }

    /// Process a committed chain - extract events and process deposits.
    fn on_commit(
        &mut self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<()> {
        let mut state = self.db.get_zone_state()?;

        // Extract all portal events from the chain
        let events = self.extract_portal_events(chain);

        for event in events {
            // Skip events we've already processed (cursor-based dedup)
            if !event.cursor.is_after(&state.cursor) {
                continue;
            }

            match &event.kind {
                PortalEventKind::Deposit(deposit) => {
                    self.handle_deposit(&event.cursor, deposit, &mut state)?;
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
            state.cursor = event.cursor;
        }

        self.db.set_zone_state(&state)?;
        Ok(())
    }

    /// Handle a deposit event - queue it and credit the recipient.
    fn handle_deposit(
        &mut self,
        cursor: &L1Cursor,
        deposit: &Deposit,
        state: &mut PzState,
    ) -> eyre::Result<()> {
        info!(
            zone_id = self.config.zone_id,
            sender = %deposit.sender,
            to = %deposit.to,
            amount = %deposit.amount,
            l1_block = cursor.block_number,
            log_index = cursor.log_index,
            "Deposit"
        );

        // Compute deposit hash for the chain
        let deposit_hash = deposit.hash(state.deposits_hash);

        // Queue deposit in DB
        self.db.queue_deposit(*cursor, deposit, deposit_hash)?;

        // Update deposits hash chain
        state.deposits_hash = deposit_hash;

        // Credit the recipient's balance immediately
        // (In a real zone, this would happen when the deposit is included in a zone block)
        self.db.upsert_account(deposit.to, |account| {
            let mut account = account.unwrap_or_default();
            account.balance += deposit.amount;
            Ok(account)
        })?;

        Ok(())
    }

    /// Handle a chain revert - undo deposits and state changes.
    fn on_revert(
        &mut self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<()> {
        let mut state = self.db.get_zone_state()?;

        // Extract events in reverse order
        let mut events = self.extract_portal_events(chain);
        events.reverse();

        for event in events {
            // Only revert events we've actually processed
            if !state.cursor.is_after(&event.cursor) && state.cursor != event.cursor {
                continue;
            }

            match &event.kind {
                PortalEventKind::Deposit(deposit) => {
                    warn!(
                        zone_id = self.config.zone_id,
                        to = %deposit.to,
                        amount = %deposit.amount,
                        l1_block = event.cursor.block_number,
                        "Reverting deposit"
                    );

                    // Debit the recipient's balance
                    self.db.upsert_account(deposit.to, |account| {
                        let mut account =
                            account.ok_or_else(|| eyre::eyre!("account not found"))?;
                        account.balance = account.balance.saturating_sub(deposit.amount);
                        Ok(account)
                    })?;
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
            self.db.remove_deposits_after(revert_cursor)?;

            // Update state cursor
            state.cursor = revert_cursor;
            self.db.set_zone_state(&state)?;
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
                    ) {
                        if deposit_event.zoneId == self.config.zone_id {
                            let deposit = Deposit {
                                l1_block_hash: deposit_event.l1BlockHash,
                                l1_block_number: deposit_event.l1BlockNumber,
                                l1_timestamp: deposit_event.l1Timestamp,
                                sender: deposit_event.sender,
                                to: deposit_event.to,
                                amount: deposit_event.amount,
                                memo: deposit_event.memo,
                            };
                            events.push(PortalEvent {
                                cursor,
                                kind: PortalEventKind::Deposit(deposit),
                            });
                        }
                    }

                    // Try to decode as batch submitted event
                    if let Ok(batch_event) =
                        BatchSubmitted::decode_raw_log(log.topics().iter().copied(), &log.data.data)
                    {
                        if batch_event.zoneId == self.config.zone_id {
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
    db_path: &str,
    config: PzConfig,
) -> impl FnOnce(
    ExExContext<Node>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = eyre::Result<()>> + Send>>
where
    Node: FullNodeComponents + 'static,
{
    let db_path = db_path.to_string();
    move |ctx| {
        Box::pin(async move {
            let connection = rusqlite::Connection::open(&db_path)?;
            let db = Database::new(connection)?;
            let exex = PrivacyZoneExEx::new(ctx, db, config);
            exex.start().await
        })
    }
}
