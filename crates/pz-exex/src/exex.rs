//! Privacy Zone Execution Extension.
//!
//! Based on reth-exex-examples/rollup pattern.

use crate::{
    db::Database,
    types::{BatchSubmitted, DepositEnqueued, PzConfig},
};
use alloy_sol_types::SolEvent;
use futures::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_tracing::tracing::{info, warn};

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

        while let Some(notification) = self.ctx.notifications.try_next().await? {
            // Handle reverts first
            if let Some(reverted_chain) = notification.reverted_chain() {
                self.revert(&reverted_chain)?;
            }

            // Then handle commits
            if let Some(committed_chain) = notification.committed_chain() {
                self.commit(&committed_chain)?;
                self.ctx
                    .events
                    .send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
            }
        }

        Ok(())
    }

    /// Process a committed chain - decode events and handle deposits.
    fn commit(
        &mut self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<()> {
        let events = self.decode_portal_events(chain);

        for (block_number, _log_index, event) in events {
            match event {
                PortalEvent::Deposit(deposit_event) => {
                    self.handle_deposit(&deposit_event, block_number)?;
                }
                PortalEvent::BatchSubmitted(batch_event) => {
                    self.handle_batch(&batch_event, block_number)?;
                }
            }
        }

        Ok(())
    }

    /// Handle a deposit event - credit the recipient account.
    fn handle_deposit(
        &mut self,
        event: &DepositEnqueued,
        block_number: u64,
    ) -> eyre::Result<()> {
        info!(
            zone_id = self.config.zone_id,
            sender = %event.sender,
            to = %event.to,
            amount = %event.amount,
            l1_block = block_number,
            "Deposit"
        );

        // Credit the recipient's balance
        self.db.upsert_account(event.to, |account| {
            let mut account = account.unwrap_or_default();
            account.balance += event.amount;
            Ok(account)
        })?;

        Ok(())
    }

    /// Handle a batch submitted event.
    fn handle_batch(
        &mut self,
        event: &BatchSubmitted,
        block_number: u64,
    ) -> eyre::Result<()> {
        info!(
            zone_id = self.config.zone_id,
            batch_index = event.batchIndex,
            state_root = %event.newStateRoot,
            l1_block = block_number,
            "Batch submitted"
        );
        Ok(())
    }

    /// Revert a chain - undo deposits.
    fn revert(
        &mut self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<()> {
        let mut events = self.decode_portal_events(chain);
        events.reverse(); // Revert in reverse order

        for (block_number, _log_index, event) in events {
            match event {
                PortalEvent::Deposit(deposit_event) => {
                    warn!(
                        zone_id = self.config.zone_id,
                        to = %deposit_event.to,
                        amount = %deposit_event.amount,
                        l1_block = block_number,
                        "Reverting deposit"
                    );

                    // Subtract from the recipient's balance
                    self.db.upsert_account(deposit_event.to, |account| {
                        let mut account = account.ok_or_else(|| eyre::eyre!("account not found"))?;
                        account.balance = account.balance.saturating_sub(deposit_event.amount);
                        Ok(account)
                    })?;
                }
                PortalEvent::BatchSubmitted(batch_event) => {
                    warn!(
                        zone_id = self.config.zone_id,
                        batch_index = batch_event.batchIndex,
                        l1_block = block_number,
                        "Reverting batch"
                    );
                }
            }
        }

        Ok(())
    }

    /// Decode portal events from a chain.
    fn decode_portal_events(
        &self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> Vec<(u64, u64, PortalEvent)> {
        use alloy_consensus::{BlockHeader as _, TxReceipt};

        let mut events = Vec::new();

        for (block, receipts) in chain.blocks_and_receipts() {
            let block_number = block.header().number();
            let mut log_index = 0u64;

            for receipt in receipts.iter() {
                for log in receipt.logs() {
                    if log.address != self.config.portal_address {
                        log_index += 1;
                        continue;
                    }

                    // Try to decode as deposit event
                    if let Ok(deposit_event) = DepositEnqueued::decode_raw_log(
                        log.topics().iter().copied(),
                        &log.data.data,
                    ) {
                        if deposit_event.zoneId == self.config.zone_id {
                            events.push((block_number, log_index, PortalEvent::Deposit(deposit_event)));
                        }
                    }

                    // Try to decode as batch submitted event
                    if let Ok(batch_event) = BatchSubmitted::decode_raw_log(
                        log.topics().iter().copied(),
                        &log.data.data,
                    ) {
                        if batch_event.zoneId == self.config.zone_id {
                            events.push((block_number, log_index, PortalEvent::BatchSubmitted(batch_event)));
                        }
                    }

                    log_index += 1;
                }
            }
        }

        events
    }
}

/// Portal events that the ExEx processes.
enum PortalEvent {
    Deposit(DepositEnqueued),
    BatchSubmitted(BatchSubmitted),
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
