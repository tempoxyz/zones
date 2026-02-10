//! Zone L2 block monitor.
//!
//! Watches the **Zone L2** chain for new blocks, collecting withdrawal events and
//! reading on-chain state to produce [`BatchData`] for the L1 batch submitter.
//!
//! All RPC calls in this module target the zone L2 node — never Tempo L1.
//!
//! ## Data produced
//!
//! - **[`BatchData`]** — sent over an unbounded channel to the batch submitter, which
//!   posts it to the ZonePortal on L1.
//! - **Withdrawals** — extracted from `WithdrawalRequested` events and stored in the
//!   [`SharedWithdrawalStore`] so the withdrawal processor can later call
//!   `processWithdrawal` on L1.

use std::time::Duration;

use alloy_primitives::{Address, B256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tracing::{debug, error, info, instrument, warn};

use crate::abi::{self, TempoState, ZoneInbox, ZoneOutbox};
use crate::batch::BatchData;
use crate::withdrawals::SharedWithdrawalStore;

/// Configuration for the [`ZoneMonitor`].
#[derive(Debug, Clone)]
pub struct ZoneMonitorConfig {
    /// ZoneOutbox contract address on Zone L2.
    pub outbox_address: Address,
    /// ZoneInbox contract address on Zone L2.
    pub inbox_address: Address,
    /// TempoState predeploy address on Zone L2 (usually [`abi::TEMPO_STATE_ADDRESS`]).
    pub tempo_state_address: Address,
    /// Zone L2 RPC URL (HTTP).
    pub zone_rpc_url: String,
    /// How often to poll for new zone blocks.
    pub poll_interval: Duration,
}

/// Monitors the Zone L2 chain for new blocks, producing [`BatchData`] and
/// populating the [`SharedWithdrawalStore`].
///
/// In the current POC every zone block corresponds to exactly one batch.
pub struct ZoneMonitor {
    config: ZoneMonitorConfig,
    /// Read-only HTTP provider pointed at the **Zone L2** RPC node.
    provider: DynProvider<TempoNetwork>,
    /// ZoneOutbox contract on **Zone L2** — source of `WithdrawalRequested` and
    /// `BatchFinalized` events.
    outbox: ZoneOutbox::ZoneOutboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// ZoneInbox contract on **Zone L2** — queried for the processed deposit queue hash.
    inbox: ZoneInbox::ZoneInboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// TempoState predeploy on **Zone L2** — provides the latest Tempo L1 block number
    /// as seen by the zone.
    tempo_state: TempoState::TempoStateInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// Shared store for withdrawal data, written here and consumed by the
    /// [`WithdrawalProcessor`](crate::withdrawals::WithdrawalProcessor) on **Tempo L1**.
    withdrawal_store: SharedWithdrawalStore,
    /// Channel sender for [`BatchData`], consumed by the
    /// [`BatchSubmitter`](crate::batch::BatchSubmitter) which posts batches to **Tempo L1**.
    batch_tx: tokio::sync::mpsc::UnboundedSender<BatchData>,
    /// Last **Zone L2** block number that was fully processed.
    last_processed_block: u64,
    /// Deposit queue hash from the previous block, used to construct the
    /// [`DepositQueueTransition`](crate::abi::DepositQueueTransition) for each batch.
    prev_processed_deposit_hash: B256,
}

impl ZoneMonitor {
    /// Create a new zone monitor.
    ///
    /// Builds a read-only HTTP provider (no wallet) pointed at the Zone L2 RPC
    /// and instantiates the on-chain contract handles.
    pub fn new(
        config: ZoneMonitorConfig,
        batch_tx: tokio::sync::mpsc::UnboundedSender<BatchData>,
        withdrawal_store: SharedWithdrawalStore,
    ) -> Self {
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http(config.zone_rpc_url.parse().expect("valid Zone RPC URL"))
            .erased();

        let outbox = ZoneOutbox::new(config.outbox_address, provider.clone());
        let inbox = ZoneInbox::new(config.inbox_address, provider.clone());
        let tempo_state = TempoState::new(config.tempo_state_address, provider.clone());

        Self {
            config,
            provider,
            outbox,
            inbox,
            tempo_state,
            withdrawal_store,
            batch_tx,
            last_processed_block: 0,
            prev_processed_deposit_hash: B256::ZERO,
        }
    }

    /// Run the monitor loop. This method never returns under normal operation.
    #[instrument(skip_all, fields(
        outbox = %self.config.outbox_address,
        inbox = %self.config.inbox_address,
    ))]
    pub async fn run(&mut self) -> Result<()> {
        info!(
            zone_rpc = %self.config.zone_rpc_url,
            "Zone monitor started"
        );

        loop {
            match self.provider.get_block_number().await {
                Ok(latest) => {
                    if latest > self.last_processed_block {
                        let from = self.last_processed_block + 1;
                        for block_number in from..=latest {
                            if let Err(e) = self.process_block(block_number).await {
                                error!(block_number, error = %e, "Failed to process zone block");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to query zone L2 block number");
                }
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Process a single zone block.
    ///
    /// In the POC every zone block is one batch. This method:
    /// 1. Collects `WithdrawalRequested` events and stores them.
    /// 2. Reads `BatchFinalized` to get the withdrawal queue hash and batch index.
    /// 3. Reads on-chain state from `TempoState` and `ZoneInbox`.
    /// 4. Constructs and sends [`BatchData`] to the batch submitter channel.
    #[instrument(skip(self), fields(block_number))]
    async fn process_block(&mut self, block_number: u64) -> Result<()> {
        debug!(block_number, "Processing zone block");

        // --- 1. Collect withdrawal events ---
        let withdrawal_events = self
            .outbox
            .WithdrawalRequested_filter()
            .from_block(block_number)
            .to_block(block_number)
            .query()
            .await?;

        // --- 2. Read BatchFinalized event ---
        let batch_finalized_events = self
            .outbox
            .BatchFinalized_filter()
            .from_block(block_number)
            .to_block(block_number)
            .query()
            .await?;

        let (withdrawal_queue_hash, withdrawal_batch_index) =
            if let Some((event, _log)) = batch_finalized_events.first() {
                (event.withdrawalQueueHash, Some(event.withdrawalBatchIndex))
            } else {
                (B256::ZERO, None)
            };

        // Store withdrawals under the batch index from BatchFinalized.
        if let Some(batch_index) = withdrawal_batch_index {
            if !withdrawal_events.is_empty() {
                let mut store = self.withdrawal_store.lock();
                for (event, _log) in &withdrawal_events {
                    let withdrawal = abi::Withdrawal {
                        sender: event.sender,
                        to: event.to,
                        amount: event.amount,
                        fee: event.fee,
                        memo: event.memo,
                        gasLimit: event.gasLimit,
                        fallbackRecipient: event.fallbackRecipient,
                        callbackData: event.data.clone(),
                    };
                    store.add_withdrawal(batch_index, withdrawal);
                }
                info!(
                    block_number,
                    batch_index,
                    count = withdrawal_events.len(),
                    "Stored withdrawals for batch"
                );
            }
        }

        // --- 3. Read zone state ---
        let tempo_block_number = self
            .tempo_state
            .tempoBlockNumber()
            .call()
            .await?;

        let next_processed_deposit_hash = self
            .inbox
            .processedDepositQueueHash()
            .call()
            .await?;

        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .await?
            .ok_or_else(|| eyre::eyre!("zone block {block_number} not found"))?;

        let block_hash = block.header.hash;
        let parent_hash = block.header.inner.inner.inner.parent_hash;

        // --- 4. Build and send BatchData ---
        let batch_data = BatchData {
            tempo_block_number,
            prev_block_hash: parent_hash,
            next_block_hash: block_hash,
            prev_processed_deposit_hash: self.prev_processed_deposit_hash,
            next_processed_deposit_hash,
            withdrawal_queue_hash,
        };

        self.prev_processed_deposit_hash = next_processed_deposit_hash;
        self.last_processed_block = block_number;

        if let Err(e) = self.batch_tx.send(batch_data) {
            error!(block_number, error = %e, "Failed to send BatchData — receiver dropped");
        } else {
            info!(
                block_number,
                tempo_block_number,
                %block_hash,
                %withdrawal_queue_hash,
                "Produced BatchData for zone block"
            );
        }

        Ok(())
    }
}

/// Spawn the zone monitor as a background task.
///
/// The monitor polls the Zone L2 for new blocks, extracts withdrawal events into the
/// [`SharedWithdrawalStore`], and sends [`BatchData`] to the batch submitter channel.
pub fn spawn_zone_monitor(
    config: ZoneMonitorConfig,
    batch_tx: tokio::sync::mpsc::UnboundedSender<BatchData>,
    withdrawal_store: SharedWithdrawalStore,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut monitor = ZoneMonitor::new(config, batch_tx, withdrawal_store);
        loop {
            if let Err(e) = monitor.run().await {
                error!(error = %e, "Zone monitor failed, restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    })
}
