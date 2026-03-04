//! Zone Engine — L1-event-driven block production for zone nodes.
//!
//! Advances the zone chain whenever new L1 blocks arrive in the deposit
//! queue, enabling full-speed sync during catch-up and instant reaction in
//! steady state.
//!
//! ## Block production flow
//!
//! ```text
//! L1Subscriber ──enqueue──► DepositQueue ──notify──► ZoneEngine
//!                                │                       │
//!                                │                   1. peek queue → L1 block
//!                                │                   2. build ZonePayloadAttributes
//!                                │                      (inner attrs + l1_block)
//!                                │                   3. FCU w/ payload attributes
//!                                │                       │
//!                                │                       ▼
//!                                │               reth payload service
//!                                │                       │
//!                                │               4. build payload
//!                                │                  (L1 data from attributes)
//!                                │                       │
//!                                │                       ▼
//!                                │                  ZoneEngine
//!                                │               5. resolve payload
//!                                │               6. newPayload
//!                                │               7. FCU (update head)
//!                                │                       │
//!                                ◄── confirm ◄───────────┘
//! ```
//!
//! The deposit queue uses a **peek / confirm** pattern: the engine peeks at
//! the next L1 block, wraps it into [`ZonePayloadAttributes`], and only
//! confirms (removes) the block after `newPayload` succeeds. A failed build
//! leaves the block in the queue for retry.
//!
//! The zone assumes **instant finality** — head, safe, and finalized all point
//! to the same block.

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes as EthPayloadAttributes};
use eyre::OptionExt;
use reth_chainspec::EthereumHardforks;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{EngineApiMessageVersion, PayloadKind, PayloadTypes};
use reth_primitives_traits::SealedHeader;
use std::{sync::Arc, time::Duration};
use tempo_chainspec::spec::TempoChainSpec;
use tempo_primitives::TempoHeader;
use tracing::{error, warn};

use crate::{
    DepositQueue, L1BlockDeposits,
    payload::{ZonePayloadAttributes, ZonePayloadTypes},
};

/// Engine that drives L2 block production from L1 events.
///
/// Waits for L1 blocks in the [`DepositQueue`], then for each block:
/// 1. Peeks the L1 block from the queue
/// 2. Builds [`ZonePayloadAttributes`] wrapping inner Tempo attrs + L1 data
/// 3. Sends FCU with payload attributes to start a build
/// 4. Resolves the built payload
/// 5. Submits via `newPayload`
/// 6. Confirms the L1 block in the queue (removes it)
///
/// On failure the L1 block stays in the queue and is retried.
#[derive(Debug)]
pub struct ZoneEngine {
    /// Chain spec for hardfork checks when building attributes.
    chain_spec: Arc<TempoChainSpec>,
    /// Engine API handle for FCU and newPayload.
    to_engine: ConsensusEngineHandle<ZonePayloadTypes>,
    /// Payload builder handle.
    payload_builder: PayloadBuilderHandle<ZonePayloadTypes>,
    /// Queue of L1 blocks with their deposits.
    deposit_queue: DepositQueue,
    /// Latest block header — used as parent for the next payload and as the
    /// head/safe/finalized hash in FCU (instant finality).
    last_header: SealedHeader<TempoHeader>,
    /// Address that receives block fees.
    fee_recipient: Address,
    /// Sequencer's secp256k1 secret key for ECIES decryption of encrypted deposits.
    sequencer_key: k256::SecretKey,
    /// ZonePortal address on L1 — used as context in HKDF key derivation.
    portal_address: Address,
    /// Cache-first, RPC-fallback TIP-403 policy provider for authorization checks
    /// on encrypted deposit recipients during preparation.
    policy_provider: crate::l1_state::PolicyProvider,
}

impl ZoneEngine {
    pub fn new(
        chain_spec: Arc<TempoChainSpec>,
        to_engine: ConsensusEngineHandle<ZonePayloadTypes>,
        payload_builder: PayloadBuilderHandle<ZonePayloadTypes>,
        deposit_queue: DepositQueue,
        last_header: SealedHeader<TempoHeader>,
        fee_recipient: Address,
        sequencer_key: k256::SecretKey,
        portal_address: Address,
        policy_provider: crate::l1_state::PolicyProvider,
    ) -> Self {
        Self {
            chain_spec,
            to_engine,
            payload_builder,
            deposit_queue,
            last_header,
            fee_recipient,
            sequencer_key,
            portal_address,
            policy_provider,
        }
    }

    /// Runs the main Zone engine loop.
    ///
    /// This method never returns under normal operation. It:
    /// 1. Waits for L1 blocks to arrive in the deposit queue
    /// 2. Advances the zone chain for each available L1 block (no delay between blocks)
    /// 3. Sends periodic FCU heartbeats
    pub async fn run(mut self) {
        let mut fcu_interval = tokio::time::interval(Duration::from_secs(1));
        fcu_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                // Periodic FCU heartbeat — also drains any blocks we missed
                _ = fcu_interval.tick() => {
                    self.advance_all_available().await;
                    if let Err(e) = self.update_forkchoice_state().await {
                        error!(target: "zone::engine", "Error updating fork choice: {:?}", e);
                    }
                }
            }
        }
    }

    /// Returns the current forkchoice state.
    ///
    /// The zone has instant finality so head = safe = finalized.
    fn forkchoice_state(&self) -> ForkchoiceState {
        ForkchoiceState::same_hash(self.last_header.hash())
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

    /// Advance the chain for all available L1 blocks in the queue.
    ///
    /// During catch-up this processes blocks as fast as the EVM can execute
    /// them, with no timer delays between blocks.
    ///
    /// Reorg safety is handled upstream by the L1 subscriber, which only
    /// enqueues blocks once they are confirmed by a successor.
    async fn advance_all_available(&mut self) {
        while let Some(l1_block) = self.deposit_queue.peek() {
            if let Err(e) = self.advance(l1_block).await {
                error!(target: "zone::engine", "Error advancing the chain: {:?}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
                break;
            }
        }
    }

    /// Decrypt encrypted deposits, check TIP-403 policy authorization, and
    /// ABI-encode everything into a [`PreparedL1Block`] ready for the payload
    /// builder. Errors (e.g. policy RPC failures) are propagated so the engine
    /// retries rather than allowing unauthorized deposits through.
    async fn prepare_l1_block(
        &self,
        l1_block: L1BlockDeposits,
    ) -> eyre::Result<crate::l1::PreparedL1Block> {
        l1_block
            .prepare(
                &self.sequencer_key,
                self.portal_address,
                &self.policy_provider,
            )
            .await
    }

    /// Advance the chain by one block.
    ///
    /// Wraps the given L1 block into [`ZonePayloadAttributes`], sends FCU
    /// with those attributes, waits for the payload to be built, then submits
    /// via `newPayload`. Only confirms (removes) the L1 block from the
    /// deposit queue after `newPayload` succeeds.
    async fn advance(&mut self, l1_block: L1BlockDeposits) -> eyre::Result<()> {
        let l1_num_hash = l1_block.header.num_hash();

        // Zone block timestamp is locked to the L1 block's timestamp so the
        // two chains stay in lockstep.
        let timestamp_secs = l1_block.header.timestamp();
        let timestamp_millis_part = l1_block.header.timestamp_millis_part;

        let l1_block = self.prepare_l1_block(l1_block).await?;

        let attributes = ZonePayloadAttributes {
            inner: EthPayloadAttributes {
                timestamp: timestamp_secs,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: self.fee_recipient,
                withdrawals: self
                    .chain_spec
                    .is_shanghai_active_at_timestamp(timestamp_secs)
                    .then(Default::default),
                parent_beacon_block_root: self
                    .chain_spec
                    .is_cancun_active_at_timestamp(timestamp_secs)
                    .then_some(B256::ZERO),
            },
            timestamp_millis_part,
            l1_block,
        };

        // Send FCU with payload attributes through the engine API to trigger
        // payload building. The forkchoice state points at the current head;
        // the attributes carry the L1 block data for the new zone block.
        let res = self
            .to_engine
            .fork_choice_updated(
                self.forkchoice_state(),
                Some(attributes),
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
        let payload = ZonePayloadTypes::block_to_payload(payload.block().clone());
        let res = self.to_engine.new_payload(payload).await?;

        if !res.is_valid() {
            eyre::bail!("Invalid payload for block {block_number}")
        }

        // newPayload succeeded — confirm the L1 block in the queue so it is
        // removed. If the queue was reorged between peek and confirm, the
        // block was already purged; log a warning but still update
        // last_header since the zone chain has advanced.
        if self.deposit_queue.confirm(l1_num_hash).is_none() {
            warn!(target: "zone::engine", ?l1_num_hash, "L1 block was purged from queue during build");
        }

        // GC stale versioned entries from the policy cache. Only the engine
        // drives this — the listener must not advance past blocks the engine
        // hasn't processed yet, otherwise policy lookups for in-flight blocks
        // could return wrong results.
        self.policy_provider.cache().advance(l1_num_hash.number);

        self.last_header = header;

        // Canonicalize the new head — FCU-with-attrs above only set the
        // *previous* head as canonical; this bare FCU makes the just-built
        // block the EL's canonical head.
        if let Err(e) = self.update_forkchoice_state().await {
            error!(target: "zone::engine", "Error sending post-newPayload FCU: {:?}", e);
        }

        Ok(())
    }
}
