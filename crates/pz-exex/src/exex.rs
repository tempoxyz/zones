//! Privacy Zone Execution Extension.
//!
//! This ExEx attaches to a Tempo L1 node and processes:
//! - Deposit events from the ZonePortal contract
//! - Batch submission events to track zone state
//! - Reorgs to maintain consistency

use crate::{
    db::Database,
    error::PzError,
    types::{BatchSubmitted, Deposit, DepositEnqueued, ZoneConfig, ZoneState},
};
use alloy_primitives::B256;
use alloy_sol_types::SolEvent;
use futures::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_tracing::tracing::{debug, info, warn};

/// Privacy Zone Execution Extension.
///
/// Watches L1 for deposit and batch events from the zone portal,
/// maintains zone state in SQLite.
pub struct PrivacyZoneExEx<Node: FullNodeComponents> {
    /// ExEx context for receiving chain notifications.
    ctx: ExExContext<Node>,
    /// Zone database.
    db: Database,
    /// Zone configuration.
    config: ZoneConfig,
}

impl<Node: FullNodeComponents> PrivacyZoneExEx<Node> {
    /// Create a new Privacy Zone ExEx.
    pub fn new(ctx: ExExContext<Node>, db: Database, config: ZoneConfig) -> Self {
        Self { ctx, db, config }
    }

    /// Start processing chain notifications.
    pub async fn start(mut self) -> eyre::Result<()> {
        info!(
            zone_id = self.config.zone_id,
            portal = %self.config.portal_address,
            gas_token = %self.config.gas_token,
            "Starting Privacy Zone ExEx"
        );

        // Initialize zone state if needed
        self.init_zone_state()?;

        // Process chain notifications
        while let Some(notification) = self.ctx.notifications.try_next().await? {
            // Handle reverts first
            if let Some(reverted_chain) = notification.reverted_chain() {
                self.handle_revert(reverted_chain.range())?;
            }

            // Then handle commits
            if let Some(committed_chain) = notification.committed_chain() {
                self.handle_commit(&committed_chain)?;
                self.ctx
                    .events
                    .send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
            }
        }

        Ok(())
    }

    /// Initialize zone state from genesis if not already set.
    fn init_zone_state(&self) -> Result<(), PzError> {
        if self.db.get_zone_config()?.is_none() {
            info!(zone_id = self.config.zone_id, "Initializing zone state");
            self.db.set_zone_config(&self.config)?;

            let state = ZoneState {
                state_root: self.config.genesis_state_root,
                processed_deposits_hash: B256::ZERO,
                deposits_hash: B256::ZERO,
                batch_index: 0,
                last_l1_block: 0,
            };
            self.db.set_zone_state(&state)?;
        }
        Ok(())
    }

    /// Handle a committed chain.
    fn handle_commit(
        &self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> Result<(), PzError> {
        let mut state = self.db.get_zone_state()?;

        debug!(chain_range = ?chain.range(), "Processing committed chain");

        // Extract events from the chain
        let events = self.decode_portal_events(chain);

        for (block_number, event) in events {
            if block_number <= state.last_l1_block {
                continue;
            }

            match event {
                PortalEvent::Deposit(deposit_event) => {
                    self.handle_deposit_event(&deposit_event, block_number, &mut state)?;
                }
                PortalEvent::BatchSubmitted(batch_event) => {
                    self.handle_batch_event(&batch_event, block_number, &mut state)?;
                }
            }

            state.last_l1_block = block_number;
        }

        self.db.set_zone_state(&state)?;
        Ok(())
    }

    /// Decode portal events from a chain.
    fn decode_portal_events(
        &self,
        chain: &Chain<<Node::Types as NodeTypes>::Primitives>,
    ) -> Vec<(u64, PortalEvent)> {
        use alloy_consensus::{BlockHeader as _, TxReceipt};

        let mut events = Vec::new();

        for (block, receipts) in chain.blocks_and_receipts() {
            let block_number = block.header().number();

            for receipt in receipts.iter() {
                for log in receipt.logs() {
                    if log.address != self.config.portal_address {
                        continue;
                    }

                    // Try to decode as deposit event
                    if let Ok(deposit_event) = DepositEnqueued::decode_raw_log(
                        log.topics().iter().copied(),
                        &log.data.data,
                    ) {
                        if deposit_event.zoneId == self.config.zone_id {
                            events.push((block_number, PortalEvent::Deposit(deposit_event)));
                        }
                    }

                    // Try to decode as batch submitted event
                    if let Ok(batch_event) =
                        BatchSubmitted::decode_raw_log(log.topics().iter().copied(), &log.data.data)
                    {
                        if batch_event.zoneId == self.config.zone_id {
                            events.push((block_number, PortalEvent::BatchSubmitted(batch_event)));
                        }
                    }
                }
            }
        }

        events
    }

    /// Handle a deposit event.
    fn handle_deposit_event(
        &self,
        event: &DepositEnqueued,
        block_number: u64,
        state: &mut ZoneState,
    ) -> Result<(), PzError> {
        let deposit = Deposit {
            l1_block_hash: event.l1BlockHash,
            l1_block_number: event.l1BlockNumber,
            l1_timestamp: event.l1Timestamp,
            sender: event.sender,
            to: event.to,
            amount: event.amount,
            memo: event.memo,
        };

        let deposit_hash = event.newDepositsHash;

        info!(
            zone_id = self.config.zone_id,
            sender = %event.sender,
            to = %event.to,
            amount = %event.amount,
            deposit_hash = %deposit_hash,
            l1_block = block_number,
            "Deposit enqueued"
        );

        // Queue the deposit
        self.db.queue_deposit(&deposit, deposit_hash)?;

        // Update deposits hash chain
        state.deposits_hash = deposit_hash;

        Ok(())
    }

    /// Handle a batch submitted event.
    fn handle_batch_event(
        &self,
        event: &BatchSubmitted,
        block_number: u64,
        state: &mut ZoneState,
    ) -> Result<(), PzError> {
        info!(
            zone_id = self.config.zone_id,
            batch_index = event.batchIndex,
            state_root = %event.newStateRoot,
            deposits_hash = %event.newDepositsHash,
            exit_count = %event.exitCount,
            l1_block = block_number,
            "Batch submitted"
        );

        // Update zone state
        state.state_root = event.newStateRoot;
        state.processed_deposits_hash = event.newDepositsHash;
        state.batch_index = event.batchIndex;

        // Store batch record
        self.db.store_batch(
            event.batchIndex,
            event.newStateRoot,
            event.newDepositsHash,
            block_number,
            &format!(r#"{{"exitCount":"{}"}}"#, event.exitCount),
        )?;

        // Mark deposits as processed
        self.db.mark_deposits_processed(event.newDepositsHash)?;

        Ok(())
    }

    /// Handle a chain revert.
    fn handle_revert(&self, range: std::ops::RangeInclusive<u64>) -> Result<(), PzError> {
        let mut state = self.db.get_zone_state()?;
        let revert_from = *range.start();

        warn!(
            zone_id = self.config.zone_id,
            revert_from = revert_from,
            revert_to = *range.end(),
            "Reverting zone state"
        );

        // Revert to before this block
        if revert_from <= state.last_l1_block {
            state.last_l1_block = revert_from.saturating_sub(1);
            self.db.set_zone_state(&state)?;
        }

        Ok(())
    }
}

/// Portal events that the ExEx processes.
enum PortalEvent {
    Deposit(DepositEnqueued),
    BatchSubmitted(BatchSubmitted),
}

/// Install the Privacy Zone ExEx on a node builder.
pub fn install_pz_exex<Node>(
    db_path: &str,
    config: ZoneConfig,
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
            let db = Database::new(connection, config.zone_id)?;
            let exex = PrivacyZoneExEx::new(ctx, db, config);
            exex.start().await
        })
    }
}
