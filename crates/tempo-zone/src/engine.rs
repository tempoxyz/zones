//! Zone Engine — L1-event-driven block production for zone nodes.
//!
//! Replaces reth's `LocalMiner` with an engine that advances the zone chain
//! whenever new L1 blocks arrive in the deposit queue, enabling full-speed
//! sync during catch-up and instant reaction in steady state.

use alloy_consensus::BlockHeader as _;
use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use eyre::OptionExt;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{
    BuiltPayload, EngineApiMessageVersion, PayloadAttributesBuilder, PayloadKind, PayloadTypes,
};
use reth_primitives_traits::{HeaderTy, SealedHeaderFor};
use reth_storage_api::BlockReader;
use std::{collections::VecDeque, time::Duration};
use tracing::error;

use crate::DepositQueue;

// TODO: FIXME: this module needs to be cleaned up

/// Engine that drives L2 block production on L1 events
#[derive(Debug)]
pub struct ZoneEngine<T: PayloadTypes, B> {
    /// Attributes builder for Zone blocks
    payload_attributes_builder: B,
    /// Engine API handle for FCU and newPayload.
    // FIXME: do we need the cl handle, fcu new payload
    to_engine: ConsensusEngineHandle<T>,
    /// Payload builder handle.
    payload_builder: PayloadBuilderHandle<T>,
    /// Queue of deposit txs from L1
    deposit_queue: DepositQueue,
    /// Latest block header on L1
    last_header: SealedHeaderFor<<T::BuiltPayload as BuiltPayload>::Primitives>,
    /// Recent block hashes for forkchoice state
    last_block_hashes: VecDeque<B256>,
}

impl<T, B> ZoneEngine<T, B>
where
    T: PayloadTypes,
    B: PayloadAttributesBuilder<
            T::PayloadAttributes,
            HeaderTy<<T::BuiltPayload as BuiltPayload>::Primitives>,
        >,
{
    pub fn new(
        provider: impl BlockReader<Header = HeaderTy<<T::BuiltPayload as BuiltPayload>::Primitives>>,
        payload_attributes_builder: B,
        to_engine: ConsensusEngineHandle<T>,
        payload_builder: PayloadBuilderHandle<T>,
        deposit_queue: DepositQueue,
    ) -> Self {
        let last_header = provider
            .sealed_header(provider.best_block_number().unwrap())
            .unwrap()
            .unwrap();

        Self {
            payload_attributes_builder,
            to_engine,
            payload_builder,
            deposit_queue,
            last_block_hashes: VecDeque::from([last_header.hash()]),
            last_header,
        }
    }

    /// Runs the main Zone engine loop
    ///
    ///
    /// This method never returns under normal operation. It:
    /// 1. Waits for L1 blocks to arrive in the deposit queue
    /// 2. Advances the zone chain for each available L1 block (no delay between blocks)
    /// 3. Sends periodic FCU heartbeats
    pub async fn run(mut self) {
        let mut fcu_interval = tokio::time::interval(Duration::from_secs(1));

        // Send initial FCU to establish head
        if let Err(e) = self.update_forkchoice_state().await {
            error!(target: "zone::engine", "Error sending initial FCU: {:?}", e);
        }

        loop {
            tokio::select! {
                // Wait for new L1 blocks in the deposit queue
                _ = self.deposit_queue.notified() => {
                    self.advance_all_available().await;
                }
                // Periodic FCU heartbeat
                _ = fcu_interval.tick() => {
                    // Also check if there are pending blocks (in case we missed a notify)
                    if self.deposit_queue.pending_count() > 0 {
                        self.advance_all_available().await;
                    } else if let Err(e) = self.update_forkchoice_state().await {
                        error!(target: "zone::engine", "Error updating fork choice: {:?}", e);
                    }
                }
            }
        }
    }

    /// Advance the chain for ALL available L1 blocks in the queue.
    ///
    /// This is the key difference from `LocalMiner`: during catch-up, this
    /// processes blocks as fast as the EVM can execute them, with no timer
    /// delays between blocks.
    async fn advance_all_available(&mut self) {
        // Keep advancing as long as there are L1 blocks queued
        while self.deposit_queue.pending_count() > 0 {
            if let Err(e) = self.advance().await {
                error!(target: "zone::engine", "Error advancing the chain: {:?}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
                break;
            }
        }
    }

    /// Returns the current forkchoice state.
    fn forkchoice_state(&self) -> ForkchoiceState {
        ForkchoiceState {
            head_block_hash: *self
                .last_block_hashes
                .back()
                .expect("at least 1 block exists"),
            safe_block_hash: *self
                .last_block_hashes
                .get(self.last_block_hashes.len().saturating_sub(32))
                .expect("at least 1 block exists"),
            finalized_block_hash: *self
                .last_block_hashes
                .get(self.last_block_hashes.len().saturating_sub(64))
                .expect("at least 1 block exists"),
        }
    }

    /// Send an FCU without payload attributes (heartbeat).
    async fn update_forkchoice_state(&self) -> eyre::Result<()> {
        let state = self.forkchoice_state();
        let res = self
            .to_engine
            .fork_choice_updated(state, None, EngineApiMessageVersion::default())
            .await?;

        if !res.is_valid() {
            eyre::bail!("Invalid fork choice update {state:?}: {res:?}")
        }

        Ok(())
    }

    /// Advance the chain by one block.
    ///
    /// Sends FCU with payload attributes, waits for the payload to be built,
    /// then submits it via newPayload and updates the head.
    async fn advance(&mut self) -> eyre::Result<()> {
        let res = self
            .to_engine
            .fork_choice_updated(
                self.forkchoice_state(),
                Some(self.payload_attributes_builder.build(&self.last_header)),
                EngineApiMessageVersion::default(),
            )
            .await?;

        if res.is_invalid() {
            eyre::bail!("Invalid payload status")
        }

        let payload_id = res.payload_id.ok_or_eyre("No payload id")?;

        let Some(Ok(payload)) = self
            .payload_builder
            .resolve_kind(payload_id, PayloadKind::WaitForPending)
            .await
        else {
            eyre::bail!("No payload")
        };

        let header = payload.block().sealed_header().clone();
        let block_number = header.number();
        let payload = T::block_to_payload(payload.block().clone());
        let res = self.to_engine.new_payload(payload).await?;

        if !res.is_valid() {
            eyre::bail!("Invalid payload for block {block_number}")
        }

        self.last_block_hashes.push_back(header.hash());
        self.last_header = header;
        // Keep at most 64 blocks
        if self.last_block_hashes.len() > 64 {
            self.last_block_hashes.pop_front();
        }

        Ok(())
    }
}
