//! L1 batch submitter for the zone sequencer.
//!
//! This module handles **Tempo L1** interactions — all transactions go to the
//! [`ZonePortal`](crate::abi::ZonePortal) contract deployed on L1. The sequencer
//! signing key is used for every L1 transaction.
//!
//! [`PreparedBatch`] is produced by the zone monitor and passed to the submitter.
//!
//! # POC limitations
//!
//! Proof validation is **skipped** by the pre-T13 stub verifier. When a settlement prover is
//! configured, submissions normally carry its Nitro NSM attestation, but may use `NoProof` when
//! proving fails, the verifier rejects a proof, or its simulation fails. Without a prover,
//! submissions use `NoProof` with an empty proof.
//!
//! # Anchor modes
//!
//! | Gap | Mode | Description |
//! |-----|------|-------------|
//! | < configured effective window | Direct | Portal reads hash from EIP-2935. |
//! | ≥ configured effective window | Ancestry | Use a recent anchor and collect ancestry headers for the batch. |
//!
//! [`BatchAnchor`] handles submissions whose `tempoBlockNumber` is outside the
//! configured direct window by falling back to ancestry mode — a recent anchor
//! block plus a locally validated parent-hash header chain.

pub(crate) mod ancestry;

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, OnceLock},
    time::Duration,
};

use crate::{
    ZoneSequencerProvider,
    abi::{
        self, BatchSubmitted, BlockTransition, DepositQueueTransition, IZoneOutbox,
        LegacyBatchSubmitted, LegacyTempoAdvanced, TempoAdvanced, TokenEnablementTransition,
        ZonePortal,
    },
    attestation::{AttestationDomain, SettlementAttestation, SettlementCertificate},
    nonce_keys::SUBMIT_BATCH_NONCE_KEY,
    prover::SettlementProof,
    prover_config::active_l1_hardfork,
    settlement::ancestry::AncestryLoader,
};
use alloy_consensus::{Transaction, TxReceipt as _, transaction::TxHashRef as _};
use alloy_eips::BlockHashOrNumber;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types_eth::Filter;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use eyre::{OptionExt as _, Result, WrapErr as _};
use reth_storage_api::BlockNumReader;
use tempo_alloy::{TempoNetwork, provider::ext::TempoProviderExt, rpc::TempoCallBuilderExt};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_primitives::{Block, TempoReceipt};
use tracing::{info, instrument, warn};
use zone_chainspec::ZoneChainSpec;
use zone_prover::{ProofBundle, VerifierMode};

#[derive(Debug)]
pub enum BatchSubmitError {
    PortalAdvanced,
    PreparedAnchorInvalid(eyre::Report),
    /// The attestation was generated using a prover selected under a different L1 policy.
    ProverHardforkChanged {
        proved: TempoHardfork,
        current: TempoHardfork,
    },
    Other(eyre::Report),
}

impl From<eyre::Report> for BatchSubmitError {
    fn from(error: eyre::Report) -> Self {
        Self::Other(error)
    }
}

impl fmt::Display for BatchSubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PortalAdvanced => {
                formatter.write_str("portal already committed the batch height")
            }
            Self::PreparedAnchorInvalid(error) => error.fmt(formatter),
            Self::ProverHardforkChanged { proved, current } => write!(
                formatter,
                "prover hardfork changed from {proved} to {current}; regenerate the proof"
            ),
            Self::Other(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BatchSubmitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PreparedAnchorInvalid(error) | Self::Other(error) => Some(error.as_ref()),
            Self::PortalAdvanced | Self::ProverHardforkChanged { .. } => None,
        }
    }
}

/// EIP-2935 stores the last 8192 block hashes, so the usable window is 8191 blocks.
const DEFAULT_EIP2935_HISTORY_WINDOW: u64 = 8192 - 1;

/// Safety margin (~3 min at 500ms block time) to avoid race conditions where
/// the block falls out of the window between our check and on-chain execution.
const DEFAULT_EIP2935_SAFETY_MARGIN: u64 = 360;

/// Bounded gas for one `submitBatch` call when gas estimation is unavailable.
///
/// Estimation against state N cannot see hash(N) in EIP-2935, but a transaction submitted after
/// observing N can only execute in N+1 or later, where that hash is available. Eight certificate
/// signatures still fit comfortably within this limit.
const SUBMIT_BATCH_GAS_LIMIT: u64 = 2_000_000;

/// Maximum number of pending withdrawal slots reconstructed in one recovery page.
/// Bounds L1 topic filters and temporary withdrawal data without limiting the on-chain FIFO.
pub(crate) const WITHDRAWAL_RECOVERY_PAGE_SIZE: u64 = 100;

/// Maximum block span for one bounded log query.
///
/// Native Zone reads no longer use this limit; it remains the bound for L1 portal log recovery.
pub(crate) const LOG_QUERY_BLOCK_CHUNK: u64 = 5_000;

/// Canonical local identity of the Zone block most recently accepted by the L1 portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalZoneAnchor {
    pub block_hash: B256,
    pub block_number: u64,
}

/// Read the L1 portal tip and resolve it against the local canonical Zone chain.
///
/// A zero portal hash denotes genesis only when the portal's Zone height is also zero. A non-zero
/// hash must be present locally at exactly the portal's Zone height; silently treating an inconsistent checkpoint or a missing hash as
/// genesis could replay already-submitted history and construct an invalid transition from state
/// that the portal has superseded.
pub async fn resolve_portal_zone_anchor<P>(
    zone_provider: &P,
    portal_address: Address,
    l1_provider: &DynProvider<TempoNetwork>,
) -> Result<PortalZoneAnchor>
where
    P: BlockNumReader,
{
    let portal = ZonePortal::new(portal_address, l1_provider);
    // Read both fields from the same L1 state so a concurrent submission cannot mix checkpoints.
    let (block_hash, zone_height) = l1_provider
        .multicall()
        .add(portal.blockHash())
        .add(portal.zoneHeight())
        .aggregate()
        .await
        .wrap_err("failed to read ZonePortal checkpoint")?;

    let block_number = if block_hash.is_zero() {
        eyre::ensure!(
            zone_height.is_zero(),
            "inconsistent ZonePortal checkpoint: zero block hash at nonzero Zone height {zone_height}"
        );
        0
    } else {
        let block_number = zone_provider.block_number(block_hash)?.ok_or_eyre(format!(
            "portal block hash {block_hash} is not canonical in the Zone node"
        ))?;
        eyre::ensure!(
            U256::from(block_number) == zone_height,
            "inconsistent ZonePortal checkpoint: block hash {block_hash} is local Zone block {block_number}, but portal Zone height is {zone_height}"
        );
        block_number
    };

    Ok(PortalZoneAnchor {
        block_hash,
        block_number,
    })
}

/// EIP-2935 anchor limits used by the batch submitter.
///
/// Production uses the real 8191-block EIP-2935 history window with a safety
/// margin. This type exists primarily so tests can shrink that otherwise large
/// window and exercise ancestry behavior without mining thousands of L1 blocks.
/// Production code should normally use [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchAnchorConfig {
    /// Total L1 block-hash history window to treat as available for EIP-2935
    /// anchoring.
    history_window: u64,
    /// Number of most-recent L1 blocks to avoid when choosing an anchor, reducing
    /// the chance that an anchor ages out before the on-chain transaction lands.
    safety_margin: u64,
}

impl BatchAnchorConfig {
    /// Build an anchor config with explicit limits.
    pub fn new(history_window: u64, safety_margin: u64) -> Result<Self> {
        if history_window == 0 {
            return Err(eyre::eyre!("EIP-2935 history window must be non-zero"));
        }

        if safety_margin >= history_window {
            return Err(eyre::eyre!(
                "EIP-2935 safety margin ({safety_margin}) must be smaller than history window ({history_window})"
            ));
        }

        Ok(Self {
            history_window,
            safety_margin,
        })
    }

    /// Configured history window in L1 blocks.
    pub const fn history_window(self) -> u64 {
        self.history_window
    }

    /// Configured safety margin in L1 blocks.
    pub const fn safety_margin(self) -> u64 {
        self.safety_margin
    }

    /// Effective direct-submission window after subtracting the safety margin.
    pub const fn effective_window(self) -> u64 {
        self.history_window - self.safety_margin
    }
}

impl Default for BatchAnchorConfig {
    fn default() -> Self {
        Self {
            history_window: DEFAULT_EIP2935_HISTORY_WINDOW,
            safety_margin: DEFAULT_EIP2935_SAFETY_MARGIN,
        }
    }
}

/// Submits zone batches to the ZonePortal contract on Tempo L1.
///
/// Holds a contract instance pointing at the portal, backed by a shared
/// [`DynProvider`] with the sequencer's signing wallet.
pub struct BatchSubmitter {
    /// ZonePortal contract address on Tempo L1 (used in tracing spans).
    portal_address: Address,
    /// Shared L1 provider (HTTP or WS) for querying the current block number
    /// (EIP-2935 window check). The same provider backs the `portal` contract
    /// instance.
    l1_provider: DynProvider<TempoNetwork>,
    /// Canonical Tempo fork activations inherited by this Zone.
    chain_spec: Arc<ZoneChainSpec>,
    /// ZonePortal contract instance for calling `submitBatch` and reading
    /// on-chain state such as `blockHash()`.
    portal: ZonePortal::ZonePortalInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// Immutable portal and chain identifiers, populated by the first metadata multicall.
    stable_portal_metadata: OnceLock<StablePortalMetadata>,
    /// Local sequencer key used to produce a 1-of-1 TIP-1091 settlement certificate.
    signer: Option<PrivateKeySigner>,
    /// EIP-2935 history and safety-margin limits used for anchor decisions.
    anchor_config: BatchAnchorConfig,
    /// Ancestry headers for out-of-window batches. Settlement batches are submitted
    /// in order, so later requests reuse almost the entire preceding range.
    ancestry: AncestryLoader,
}
impl BatchSubmitter {
    /// Shared Tempo L1 provider backing portal reads and submissions.
    pub(crate) const fn l1_provider(&self) -> &DynProvider<TempoNetwork> {
        &self.l1_provider
    }

    /// Create a batch submitter without a certificate signer.
    ///
    /// This is useful for read-only operations and tests. Batch submission returns an error.
    pub fn new(
        portal_address: Address,
        l1_provider: DynProvider<TempoNetwork>,
        chain_spec: Arc<ZoneChainSpec>,
    ) -> Self {
        Self::with_anchor_config(
            portal_address,
            l1_provider,
            chain_spec,
            BatchAnchorConfig::default(),
        )
    }

    /// Create a new batch submitter with custom EIP-2935 anchor limits.
    pub fn with_anchor_config(
        portal_address: Address,
        l1_provider: DynProvider<TempoNetwork>,
        chain_spec: Arc<ZoneChainSpec>,
        anchor_config: BatchAnchorConfig,
    ) -> Self {
        Self::with_optional_signer_and_anchor_config(
            portal_address,
            l1_provider,
            chain_spec,
            None,
            anchor_config,
        )
    }

    /// Create a batch submitter that signs TIP-1091 settlement certificates locally.
    pub fn with_signer_and_anchor_config(
        portal_address: Address,
        l1_provider: DynProvider<TempoNetwork>,
        chain_spec: Arc<ZoneChainSpec>,
        signer: PrivateKeySigner,
        anchor_config: BatchAnchorConfig,
    ) -> Self {
        Self::with_optional_signer_and_anchor_config(
            portal_address,
            l1_provider,
            chain_spec,
            Some(signer),
            anchor_config,
        )
    }

    pub(crate) fn with_optional_signer_and_anchor_config(
        portal_address: Address,
        l1_provider: DynProvider<TempoNetwork>,
        chain_spec: Arc<ZoneChainSpec>,
        signer: Option<PrivateKeySigner>,
        anchor_config: BatchAnchorConfig,
    ) -> Self {
        let portal = ZonePortal::new(portal_address, l1_provider.clone());
        Self {
            portal_address,
            ancestry: AncestryLoader::new(l1_provider.clone()),
            l1_provider,
            chain_spec,
            portal,
            stable_portal_metadata: OnceLock::new(),
            signer,
            anchor_config,
        }
    }

    /// Resolve the single immutable anchor shared by proving, quorum, and submission.
    pub async fn prepare_batch(&self, batch: BatchData) -> Result<PreparedBatch> {
        let anchor = self.resolve_batch_anchor(batch.tempo_block_number).await?;
        Ok(PreparedBatch { batch, anchor })
    }

    /// Submit a batch to the ZonePortal on Tempo L1.
    ///
    /// Uses the anchor already selected by the zone monitor:
    ///
    /// - **Direct** — `tempo_block_number` is within the configured effective window,
    ///   the portal reads its hash directly from EIP-2935.
    /// - **Ancestry** — `tempo_block_number` is outside the effective window. A
    ///   recent anchor block is used and ancestry headers are collected (for
    ///   future prover integration).
    ///
    /// `verifier_mode` sets `verifierConfig`, which the certificate must commit to. A supplied
    /// proof must match the mode's shape; without one, an empty proof is left to the verifier.
    ///
    /// Returns the `BatchSubmitted` event decoded from the confirmed receipt.
    #[instrument(skip_all, fields(
        portal = %self.portal_address,
        tempo_block = prepared.batch.tempo_block_number,
        anchor_block = prepared.anchor_block_number(),
        prev_block_hash = %prepared.batch.prev_block_hash,
        next_block_hash = %prepared.batch.next_block_hash,
        withdrawal_queue_hash = %prepared.batch.withdrawal_queue_hash,
        withdrawal_batch_index = prepared.batch.withdrawal_batch_index,
        prover_hardfork = ?proof_bundle.map(|proof| proof.hardfork),
    ))]
    pub async fn submit_batch(
        &self,
        prepared: &PreparedBatch,
        proof_bundle: Option<&SettlementProof>,
        certificate: Option<&SettlementCertificate>,
        verifier_mode: VerifierMode,
    ) -> std::result::Result<BatchSubmitted, BatchSubmitError> {
        let active = active_l1_hardfork(&self.l1_provider, self.chain_spec.as_ref()).await?;
        if let Some(proved) = proof_bundle.map(|proof| proof.hardfork)
            && proved != active
        {
            return Err(BatchSubmitError::ProverHardforkChanged {
                proved,
                current: active,
            });
        }
        let settlement_abi = SettlementAbi::from_hardfork(active);
        let batch = &prepared.batch;
        let (verifier_config, proof) =
            settlement_proof(verifier_mode, proof_bundle.map(|proof| &proof.bundle))?;
        let block_transition = BlockTransition {
            prevBlockHash: batch.prev_block_hash,
            nextBlockHash: batch.next_block_hash,
        };
        let deposit_transition = DepositQueueTransition {
            prevProcessedHash: batch.prev_processed_deposit_hash,
            nextProcessedHash: batch.next_processed_deposit_hash,
            prevDepositNumber: batch.prev_deposit_number,
            nextDepositNumber: batch.next_deposit_number,
        };
        let token_transition = TokenEnablementTransition {
            prevProcessedTokenCount: batch.prev_processed_token_count,
            nextProcessedTokenCount: batch.next_processed_token_count,
        };

        let signer = self.signer.as_ref();
        let metadata = self
            .read_submission_metadata(signer.map_or(Address::ZERO, PrivateKeySigner::address))
            .await?;
        self.validate_submission_metadata(batch, metadata, certificate.is_some())?;
        if let Some(certificate) = certificate {
            self.validate_certificate(
                prepared,
                settlement_abi,
                batch.zone_height,
                metadata,
                verifier_mode,
                certificate,
            )?;
        }
        let current_l1_block = self.validate_prepared_anchor(prepared).await?;
        let recent_tempo_block_number = prepared.anchor.recent_block_number();
        // EIP-2935 exposes hash(N) starting in N+1. A transaction built after observing head N
        // cannot land before N+1, so anchoring to the current tip is valid at execution time.
        let anchor_block_number = prepared.anchor_block_number();
        let anchor_block_hash = prepared.anchor.block_hash();
        let anchors_to_current_tip = anchor_block_number == current_l1_block;

        let signatures = if let Some(certificate) = certificate {
            certificate.signatures.clone()
        } else {
            // Legacy mode, where the 1-of-1 sequencer will self-sign the attestation
            let signer = signer
                .ok_or_eyre("TIP-1091 batch submission requires the local sequencer signer")?;
            if !metadata.signer_is_sequencer {
                return Err(eyre::eyre!(
                    "local sequencer signer {} is not active in the portal sequencer set",
                    signer.address()
                )
                .into());
            }
            vec![self.sign_settlement_attestation(
                signer,
                metadata,
                SettlementAttestationInput {
                    batch,
                    settlement_abi,
                    anchor_block_number,
                    anchor_block_hash,
                    block_transition: &block_transition,
                    deposit_transition: &deposit_transition,
                    verifier_config: &verifier_config,
                },
            )?]
        };

        // Refetch the committed lane nonce for every submission attempt. The provider's
        // process-local nonce cache advances before a send is known to have succeeded, so
        // relying on it after a failed send can create an unfillable 2D-nonce gap.
        let submission_address = signer
            .ok_or_eyre("batch submission requires the local sequencer signer")?
            .address();
        let nonce = self
            .l1_provider
            .get_transaction_count_with_nonce_key(submission_address, SUBMIT_BATCH_NONCE_KEY)
            .await
            .map_err(|error| BatchSubmitError::Other(error.into()))?;

        info!(
            anchor_mode = prepared.anchor.mode_name(),
            anchor_block_number,
            anchor_block_hash = %anchor_block_hash,
            recent_tempo_block_number,
            current_l1_block,
            anchors_to_current_tip,
            ?verifier_mode,
            batch_prev_block_hash = %batch.prev_block_hash,
            nonce_key = ?SUBMIT_BATCH_NONCE_KEY,
            nonce,
            "Submitting batch to ZonePortal on L1"
        );

        let receipt = match settlement_abi {
            SettlementAbi::Legacy => {
                let mut submission = self
                    .portal
                    .submitBatch_0(
                        batch.tempo_block_number,
                        recent_tempo_block_number,
                        block_transition,
                        deposit_transition,
                        batch.withdrawal_queue_hash,
                        verifier_config,
                        proof,
                        U256::from(batch.zone_height),
                        signatures,
                    )
                    .nonce_key(SUBMIT_BATCH_NONCE_KEY)
                    .nonce(nonce)
                    .max_fee_per_gas(crate::TEMPO_L1_MAX_FEE_PER_GAS)
                    .max_priority_fee_per_gas(0);
                if anchors_to_current_tip {
                    submission = submission.gas(SUBMIT_BATCH_GAS_LIMIT);
                }
                tokio::time::timeout(Duration::from_secs(30), submission.send_sync()).await
            }
            SettlementAbi::T13 => {
                let mut submission = self
                    .portal
                    .submitBatch_1(
                        batch.tempo_block_number,
                        recent_tempo_block_number,
                        block_transition,
                        deposit_transition,
                        token_transition,
                        batch.withdrawal_queue_hash,
                        verifier_config,
                        proof,
                        U256::from(batch.zone_height),
                        signatures,
                    )
                    .nonce_key(SUBMIT_BATCH_NONCE_KEY)
                    .nonce(nonce)
                    .max_fee_per_gas(crate::TEMPO_L1_MAX_FEE_PER_GAS)
                    .max_priority_fee_per_gas(0);
                if anchors_to_current_tip {
                    submission = submission.gas(SUBMIT_BATCH_GAS_LIMIT);
                }
                tokio::time::timeout(Duration::from_secs(30), submission.send_sync()).await
            }
        }
        .map_err(|_| eyre::eyre!("submitBatch sync submission timed out after 30 seconds"))?
        .map_err(|error| BatchSubmitError::Other(error.into()))?;

        let tx_hash = receipt.transaction_hash();
        if !receipt.status() {
            return Err(
                eyre::eyre!("submitBatch tx {tx_hash} was included but reverted on L1").into(),
            );
        }

        let event = self.decode_batch_submitted(receipt.logs())?;

        info!(
            %tx_hash,
            withdrawal_batch_index = event.withdrawalBatchIndex,
            withdrawal_queue_index = %event.withdrawalQueueIndex,
            "Batch submitted to L1"
        );

        Ok(event)
    }

    fn sign_settlement_attestation(
        &self,
        signer: &PrivateKeySigner,
        metadata: PortalSubmissionMetadata,
        attestation: SettlementAttestationInput<'_>,
    ) -> Result<Bytes> {
        let SettlementAttestationInput {
            batch,
            settlement_abi,
            anchor_block_number,
            anchor_block_hash,
            block_transition,
            deposit_transition,
            verifier_config,
        } = attestation;
        let domain = AttestationDomain {
            l1_chain_id: metadata.stable.chain_id,
            portal_address: self.portal_address,
            zone_id: metadata.stable.zone_id,
        };
        let message = SettlementAttestation {
            zoneId: metadata.stable.zone_id,
            sequencerSetVersion: metadata.sequencer_set_version,
            zoneHeight: U256::from(batch.zone_height),
            withdrawalBatchIndex: U256::from(batch.withdrawal_batch_index),
            verifier: metadata.verifier,
            tempoBlockNumber: batch.tempo_block_number,
            anchorBlockNumber: anchor_block_number,
            anchorBlockHash: anchor_block_hash,
            blockTransitionHash: keccak256(block_transition.abi_encode()),
            depositQueueTransitionHash: keccak256(deposit_transition.abi_encode()),
            tokenEnablementTransitionHash: settlement_abi.token_transition_hash(
                batch.prev_processed_token_count,
                batch.next_processed_token_count,
            ),
            withdrawalQueueHash: batch.withdrawal_queue_hash,
            verifierConfigHash: keccak256(verifier_config),
        };
        let digest = domain.settlement_digest(&message);
        let signature = signer.sign_hash_sync(&digest)?;
        Ok(signature.as_bytes().into())
    }

    /// Read all mutable portal state needed for one submission at a single L1 block.
    ///
    /// The portal and chain identifiers are immutable, so the first call includes and caches them.
    /// Sequencer membership and verifier configuration are deliberately refreshed on every submission.
    async fn read_submission_metadata(&self, signer: Address) -> Result<PortalSubmissionMetadata> {
        if let Some(stable) = self.stable_portal_metadata.get().copied() {
            let (
                withdrawal_batch_index,
                sequencer_set_version,
                sequencer_threshold,
                signer_is_sequencer,
                verifier,
            ) = self
                .l1_provider
                .multicall()
                .add(self.portal.withdrawalBatchIndex())
                .add(self.portal.sequencerSetVersion())
                .add(self.portal.sequencerThreshold())
                .add(self.portal.isSequencer(signer))
                .add(self.portal.verifier())
                .aggregate()
                .await?;
            return Ok(Self::build_submission_metadata(
                RawPortalSubmissionMetadata {
                    withdrawal_batch_index,
                    sequencer_set_version,
                    sequencer_threshold,
                    signer_is_sequencer,
                    verifier,
                },
                stable,
            ));
        }

        let (
            withdrawal_batch_index,
            sequencer_set_version,
            sequencer_threshold,
            signer_is_sequencer,
            verifier,
            zone_id,
            chain_id,
        ) = self
            .l1_provider
            .multicall()
            .add(self.portal.withdrawalBatchIndex())
            .add(self.portal.sequencerSetVersion())
            .add(self.portal.sequencerThreshold())
            .add(self.portal.isSequencer(signer))
            .add(self.portal.verifier())
            .add(self.portal.zoneId())
            .get_chain_id()
            .aggregate()
            .await?;
        let stable = StablePortalMetadata {
            zone_id,
            chain_id: chain_id
                .try_into()
                .map_err(|_| eyre::eyre!("Tempo L1 chain ID overflow"))?,
        };
        let _ = self.stable_portal_metadata.set(stable);
        Ok(Self::build_submission_metadata(
            RawPortalSubmissionMetadata {
                withdrawal_batch_index,
                sequencer_set_version,
                sequencer_threshold,
                signer_is_sequencer,
                verifier,
            },
            stable,
        ))
    }

    fn build_submission_metadata(
        raw: RawPortalSubmissionMetadata,
        stable: StablePortalMetadata,
    ) -> PortalSubmissionMetadata {
        PortalSubmissionMetadata {
            withdrawal_batch_index: raw.withdrawal_batch_index,
            stable,
            sequencer_set_version: raw.sequencer_set_version,
            sequencer_threshold: raw.sequencer_threshold,
            signer_is_sequencer: raw.signer_is_sequencer,
            verifier: raw.verifier,
        }
    }

    fn validate_submission_metadata(
        &self,
        batch: &BatchData,
        metadata: PortalSubmissionMetadata,
        has_certificate: bool,
    ) -> Result<()> {
        let expected_l2_index = metadata
            .withdrawal_batch_index
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("portal withdrawal batch index overflow"))?;
        eyre::ensure!(
            batch.withdrawal_batch_index == expected_l2_index,
            "withdrawal batch index mismatch for zone block {}: L2 finalized index {}, expected portal index + 1 ({expected_l2_index})",
            batch.zone_height,
            batch.withdrawal_batch_index,
        );
        eyre::ensure!(
            metadata.sequencer_threshold > 0,
            "portal sequencer threshold is zero"
        );
        if !has_certificate {
            eyre::ensure!(
                metadata.sequencer_threshold == 1,
                "minimal TIP-1091 compatibility supports only a 1-of-1 sequencer set; portal threshold is {}",
                metadata.sequencer_threshold
            );
        }
        Ok(())
    }

    /// Decode the `BatchSubmitted` event from a confirmed `submitBatch` receipt's logs.
    fn decode_batch_submitted(&self, logs: &[alloy_rpc_types_eth::Log]) -> Result<BatchSubmitted> {
        logs.iter()
            .filter(|log| log.address() == self.portal_address)
            .find_map(|log| decode_batch_submitted_log(&log.inner))
            .transpose()?
            .ok_or_else(|| {
                eyre::eyre!("confirmed submitBatch receipt is missing the BatchSubmitted event")
            })
    }

    /// Validate that a collected certificate commits to the exact calldata this submitter will
    /// send, and derive the anchor mode from the signed statement instead of recomputing it.
    fn validate_certificate(
        &self,
        prepared: &PreparedBatch,
        settlement_abi: SettlementAbi,
        zone_height: u64,
        metadata: PortalSubmissionMetadata,
        verifier_mode: VerifierMode,
        certificate: &SettlementCertificate,
    ) -> Result<()> {
        let batch = &prepared.batch;
        if certificate.height != zone_height {
            return Err(eyre::eyre!(
                "settlement certificate height {} does not match batch height {zone_height}",
                certificate.height
            ));
        }
        let attestation = &certificate.attestation;

        let expected_block_transition_hash = alloy_primitives::keccak256(
            (batch.prev_block_hash, batch.next_block_hash).abi_encode(),
        );
        let expected_deposit_transition_hash = alloy_primitives::keccak256(
            (
                batch.prev_processed_deposit_hash,
                batch.next_processed_deposit_hash,
                batch.prev_deposit_number,
                batch.next_deposit_number,
            )
                .abi_encode(),
        );
        let expected_token_transition_hash = settlement_abi.token_transition_hash(
            batch.prev_processed_token_count,
            batch.next_processed_token_count,
        );

        // Run a bunch of checks to verify that whats in the attestation certificate is exactly what
        // we expect. `submitBatch` will revert if any of these are wrong, so we should catch it early.
        eyre::ensure!(
            attestation.zoneId == metadata.stable.zone_id,
            "certificate zone ID changed"
        );
        eyre::ensure!(
            attestation.sequencerSetVersion == metadata.sequencer_set_version,
            "certificate signer-set version changed"
        );
        eyre::ensure!(
            attestation.zoneHeight == U256::from(zone_height),
            "certificate zone height changed"
        );
        eyre::ensure!(
            attestation.withdrawalBatchIndex == U256::from(batch.withdrawal_batch_index),
            "certificate withdrawal batch index changed"
        );
        eyre::ensure!(
            attestation.verifier == metadata.verifier,
            "certificate verifier changed"
        );
        eyre::ensure!(
            attestation.tempoBlockNumber == batch.tempo_block_number,
            "certificate Tempo block changed"
        );
        eyre::ensure!(
            attestation.blockTransitionHash == expected_block_transition_hash,
            "certificate block transition changed"
        );
        eyre::ensure!(
            attestation.depositQueueTransitionHash == expected_deposit_transition_hash,
            "certificate deposit transition changed"
        );
        eyre::ensure!(
            attestation.tokenEnablementTransitionHash == expected_token_transition_hash,
            "certificate token transition changed"
        );
        eyre::ensure!(
            attestation.withdrawalQueueHash == batch.withdrawal_queue_hash,
            "certificate withdrawal queue hash changed"
        );
        eyre::ensure!(
            VerifierMode::try_from(attestation.verifierConfigHash)? == verifier_mode,
            "certificate verifier config changed"
        );
        eyre::ensure!(
            attestation.anchorBlockNumber == prepared.anchor_block_number(),
            "certificate anchor block changed"
        );
        eyre::ensure!(
            attestation.anchorBlockHash == prepared.anchor.block_hash(),
            "certificate anchor hash changed"
        );

        Ok(())
    }

    /// Resolve the anchor mode for the given `tempo_block_number`.
    ///
    /// - **Direct** (gap < configured effective window): the portal reads the
    ///   hash directly from EIP-2935.
    /// - **Ancestry** (gap ≥ configured effective window): a recent L1 block
    ///   behind the configured safety margin is used as anchor. Ancestry headers
    ///   are collected and validated for future prover integration.
    async fn resolve_batch_anchor(&self, tempo_block_number: u64) -> Result<BatchAnchor> {
        let current_l1_block = self.l1_provider.get_block_number().await?;
        if tempo_block_number > current_l1_block {
            return Err(eyre::eyre!(
                "tempo_block_number ({tempo_block_number}) is not yet confirmed on L1 \
                 (tip={current_l1_block}), will retry after L1 advances"
            ));
        }

        let gap = current_l1_block.saturating_sub(tempo_block_number);
        if gap < self.anchor_config.effective_window() {
            self.ancestry.clear();
            let block_hash = self
                .l1_provider
                .get_header_by_number(tempo_block_number.into())
                .await?
                .ok_or_eyre(format!("L1 anchor block {tempo_block_number} not found"))?
                .inner
                .hash;
            return Ok(BatchAnchor::Direct { block_hash });
        }

        let anchor_block = current_l1_block.saturating_sub(self.anchor_config.safety_margin());
        let expected = self
            .l1_provider
            .get_header_by_number(anchor_block.into())
            .await?
            .ok_or_eyre(format!("L1 anchor block {anchor_block} not found"))?;
        let check = |resolved: &ancestry::Ancestry| {
            eyre::ensure!(
                resolved.anchor.hash == expected.inner.hash,
                "L1 anchor {anchor_block} hash {} does not match canonical hash {}",
                resolved.anchor.hash,
                expected.inner.hash
            );
            Ok(())
        };
        let resolved = match self
            .ancestry
            .load(tempo_block_number, anchor_block, check)
            .await
        {
            Err(_) => {
                self.ancestry.clear();
                self.ancestry
                    .load(tempo_block_number, anchor_block, check)
                    .await?
            }
            Ok(resolved) => resolved,
        };
        warn!(
            tempo_block_number,
            current_l1_block,
            anchor_block,
            gap,
            header_count = resolved.headers.len(),
            total_bytes = resolved.headers.iter().map(|h| h.len()).sum::<usize>(),
            "tempo_block_number outside EIP-2935 effective window, using ancestry mode"
        );

        Ok(resolved.into())
    }

    async fn validate_prepared_anchor(
        &self,
        prepared: &PreparedBatch,
    ) -> std::result::Result<u64, BatchSubmitError> {
        let anchor = &prepared.anchor;
        let anchor_block_number = prepared.anchor_block_number();
        if anchor_block_number < prepared.batch.tempo_block_number {
            return Err(BatchSubmitError::PreparedAnchorInvalid(eyre::eyre!(
                "prepared L1 anchor predates the batch Tempo block"
            )));
        }

        let current_l1_block = self
            .l1_provider
            .get_block_number()
            .await
            .map_err(|error| BatchSubmitError::Other(error.into()))?;
        if anchor_block_number > current_l1_block {
            return Err(BatchSubmitError::Other(eyre::eyre!(
                "prepared L1 anchor block is ahead of the current L1 tip"
            )));
        }
        if current_l1_block.saturating_sub(anchor_block_number)
            >= self.anchor_config.history_window()
        {
            return Err(BatchSubmitError::PreparedAnchorInvalid(eyre::eyre!(
                "prepared L1 anchor block fell outside the EIP-2935 history window"
            )));
        }

        let canonical = self
            .l1_provider
            .get_header_by_number(anchor_block_number.into())
            .await
            .map_err(|error| BatchSubmitError::Other(error.into()))?
            .ok_or_else(|| {
                BatchSubmitError::Other(eyre::eyre!(
                    "prepared L1 anchor block {} not found",
                    anchor_block_number
                ))
            })?;
        if canonical.inner.hash != anchor.block_hash() {
            return Err(BatchSubmitError::PreparedAnchorInvalid(eyre::eyre!(
                "prepared L1 anchor hash is no longer canonical"
            )));
        }
        Ok(current_l1_block)
    }

    /// Read the current `blockHash` from the ZonePortal on L1.
    ///
    /// Used to reject a prepared batch whose predecessor no longer matches the
    /// portal before submitting it.
    pub async fn read_portal_block_hash(&self) -> Result<B256> {
        let hash = self.portal.blockHash().call().await?;
        Ok(hash)
    }

    /// Read the current withdrawal queue bounds in one Multicall3 request.
    async fn read_portal_withdrawal_queue_bounds(&self) -> Result<(u64, u64)> {
        let (head, tail) = self
            .l1_provider
            .multicall()
            .add(self.portal.withdrawalQueueHead())
            .add(self.portal.withdrawalQueueTail())
            .aggregate()
            .await?;
        Ok((
            head.try_into()
                .map_err(|_| eyre::eyre!("withdrawal queue head overflow"))?,
            tail.try_into()
                .map_err(|_| eyre::eyre!("withdrawal queue tail overflow"))?,
        ))
    }

    /// Re-populate one page of the in-memory [`WithdrawalStore`](crate::withdrawals::WithdrawalStore)
    /// after a sequencer restart or page refill.
    ///
    /// The L1 portal stores only hash chains, not the actual [`Withdrawal`](abi::Withdrawal)
    /// structs. This method reconstructs them by:
    ///
    /// 1. Reading `withdrawalQueueHead` / `withdrawalQueueTail` from the **L1 portal**
    ///    to determine which bounded page starts at the current head.
    /// 2. Querying the `BatchSubmitted` event for each slot in that page via the
    ///    indexed `withdrawalQueueIndex` topic.
    /// 3. Resolving each event's `nextBlockHash` to a **zone L2** block number.
    /// 4. Fetching `WithdrawalRequested` events from the **zone L2** outbox in
    ///    that block. A non-empty withdrawal batch is finalized in the same
    ///    zone block as its requests.
    /// 5. Reading the head slot's current on-chain hash for partial processing
    ///    detection.
    /// 6. Verifying the hash chain and trimming already-processed withdrawals.
    ///
    /// Returns one bounded page of verified withdrawals starting at the portal head.
    #[instrument(skip_all, fields(portal = %self.portal_address))]
    pub async fn fetch_pending_withdrawals<P: ZoneSequencerProvider>(
        &self,
        zone_provider: &P,
        outbox_address: Address,
    ) -> Result<WithdrawalPage> {
        // Step 1: read pending slot range from the L1 portal and bound this recovery page.
        let (head, tail) = self.read_portal_withdrawal_queue_bounds().await?;
        let page_tail = tail.min(head.saturating_add(WITHDRAWAL_RECOVERY_PAGE_SIZE));

        if head >= tail {
            return Ok(WithdrawalPage {
                head,
                tail: head,
                batches: BTreeMap::new(),
            });
        }

        // Step 2: query BatchSubmitted events for this page [head, page_tail)
        // by their indexed withdrawalQueueIndex.
        let events = self.find_batch_events_by_index(head, page_tail).await?;

        // Step 3: resolve each L1 event's nextBlockHash to a zone L2 block number.
        let mut zone_block_by_slot: BTreeMap<u64, u64> = BTreeMap::new();
        for (&portal_slot, event) in &events {
            let block_number = zone_provider
                .block_number(event.nextBlockHash)?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "zone block not found for hash {} (portal slot {portal_slot})",
                        event.nextBlockHash
                    )
                })?;
            zone_block_by_slot.insert(portal_slot, block_number);
        }

        // Step 4: fetch WithdrawalRequested events from zone L2 for each pending slot.
        let mut slot_withdrawals: BTreeMap<u64, Vec<abi::Withdrawal>> = BTreeMap::new();
        for portal_slot in head..page_tail {
            let Some(&zone_block) = zone_block_by_slot.get(&portal_slot) else {
                continue;
            };
            let withdrawals =
                fetch_slot_withdrawals(zone_provider, outbox_address, zone_block).await?;
            slot_withdrawals.insert(portal_slot, withdrawals);
        }

        // Step 5: read the head slot's current on-chain hash (for partial processing detection).
        let head_slot_hash = self
            .portal
            .withdrawalQueueSlot(U256::from(head))
            .call()
            .await?;

        // Guard: verify the queue didn't change during the multi-RPC replay.
        let (head2, tail2) = self.read_portal_withdrawal_queue_bounds().await?;

        if head2 != head || tail2 < page_tail {
            eyre::bail!(
                "withdrawal queue changed during page restore ({}..{} -> {}..{}), retry from the current head",
                head,
                tail,
                head2,
                tail2
            );
        }

        // Step 6: resolve all fetched data into verified withdrawal sets.
        resolve_pending_slots(head, page_tail, &events, &slot_withdrawals, head_slot_hash).map(
            |batches| WithdrawalPage {
                head,
                tail: tail2,
                batches,
            },
        )
    }

    /// Fetch `BatchSubmitted` events for logical queue indices `[first_index, tail)`
    /// by walking L1 backwards in chunks while filtering by the indexed
    /// `withdrawalQueueIndex` topic. Logical queue indices never repeat
    /// (head/tail are non-wrapping counters), so the topic filter identifies
    /// each batch exactly without positional counting.
    async fn find_batch_events_by_index(
        &self,
        first_index: u64,
        tail: u64,
    ) -> Result<BTreeMap<u64, BatchSubmitted>> {
        if first_index >= tail {
            return Ok(BTreeMap::new());
        }

        let index_topics: Vec<B256> = (first_index..tail)
            .map(|index| B256::from(U256::from(index)))
            .collect();
        let needed = index_topics.len();

        let mut found = BTreeMap::new();
        let mut hi = self.l1_provider.get_block_number().await?;

        while found.len() < needed {
            let lo = backward_log_query_start(hi, 0);

            let filter = Filter::new()
                .address(self.portal_address)
                .event_signature(vec![
                    BatchSubmitted::SIGNATURE_HASH,
                    LegacyBatchSubmitted::SIGNATURE_HASH,
                ])
                .topic2(index_topics.clone())
                .from_block(lo)
                .to_block(hi);
            let events = self.l1_provider.get_logs(&filter).await?;

            for log in events {
                let event = decode_batch_submitted_log(&log.inner)
                    .transpose()?
                    .ok_or_else(|| eyre::eyre!("unexpected event in BatchSubmitted query"))?;
                let index: u64 = event.withdrawalQueueIndex.try_into().map_err(|_| {
                    eyre::eyre!("withdrawal queue index overflow in BatchSubmitted")
                })?;
                if found.insert(index, event).is_some() {
                    eyre::bail!("duplicate BatchSubmitted event for portal queue index {index}");
                }
            }

            if lo == 0 {
                break;
            }
            hi = lo - 1;
        }

        Ok(found)
    }
}

fn decode_batch_submitted_log(log: &alloy_primitives::Log) -> Option<Result<BatchSubmitted>> {
    match log.topics().first() {
        Some(topic) if topic == &BatchSubmitted::SIGNATURE_HASH => Some(
            BatchSubmitted::decode_log(log)
                .map(|event| event.data)
                .map_err(Into::into),
        ),
        Some(topic) if topic == &LegacyBatchSubmitted::SIGNATURE_HASH => Some(
            LegacyBatchSubmitted::decode_log(log)
                .map(|event| BatchSubmitted {
                    withdrawalBatchIndex: event.withdrawalBatchIndex,
                    withdrawalQueueIndex: event.withdrawalQueueIndex,
                    nextProcessedDepositQueueHash: event.nextProcessedDepositQueueHash,
                    nextBlockHash: event.nextBlockHash,
                    withdrawalQueueHash: event.withdrawalQueueHash,
                    lastProcessedDepositNumber: event.lastProcessedDepositNumber,
                    lastProcessedEnabledTokenCount: 0,
                })
                .map_err(Into::into),
        ),
        _ => None,
    }
}

/// Verified bounded window of the portal withdrawal queue reconstructed from chain history.
#[derive(Debug)]
pub struct WithdrawalPage {
    /// Portal head observed before reconstructing the page.
    pub head: u64,
    /// Portal tail observed after reconstruction was validated.
    pub tail: u64,
    /// Verified payloads keyed by logical portal queue index.
    pub batches: BTreeMap<u64, Vec<abi::Withdrawal>>,
}

/// Settlement ABI selected by the live L1 hardfork at submission time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementAbi {
    /// Pre-T13 selector and EIP-712 statement.
    Legacy,
    /// T13 selector and token-enablement-bound EIP-712 statement.
    T13,
}

impl SettlementAbi {
    /// Resolve the settlement selector and attestation format from the live Tempo L1 hardfork.
    pub async fn from_l1(
        provider: &DynProvider<TempoNetwork>,
        chain_spec: &ZoneChainSpec,
    ) -> Result<Self> {
        Ok(Self::from_hardfork(
            active_l1_hardfork(provider, chain_spec).await?,
        ))
    }

    fn from_hardfork(hardfork: TempoHardfork) -> Self {
        if hardfork >= TempoHardfork::T13 {
            Self::T13
        } else {
            Self::Legacy
        }
    }

    /// Hash the token transition exactly as the selected settlement statement expects.
    pub fn token_transition_hash(self, previous: u64, next: u64) -> B256 {
        match self {
            Self::Legacy => B256::ZERO,
            Self::T13 => keccak256((previous, next).abi_encode()),
        }
    }
}

/// Data required to submit a single batch to the ZonePortal on L1.
///
/// Produced by the zone monitor and combined with one [`BatchAnchor`] in [`PreparedBatch`].
#[derive(Debug, Clone)]
pub struct BatchData {
    /// Zone L2 height committed by this batch.
    pub zone_height: u64,
    /// Tempo L1 block number for EIP-2935 verification.
    pub tempo_block_number: u64,
    /// Previous zone block hash (must match portal's current `blockHash`).
    pub prev_block_hash: B256,
    /// New zone block hash after this batch.
    pub next_block_hash: B256,
    /// Deposit queue: where the zone started processing.
    pub prev_processed_deposit_hash: B256,
    /// Deposit queue: where the zone processed up to.
    pub next_processed_deposit_hash: B256,
    /// Deposit counter at the start of processing.
    pub prev_deposit_number: u64,
    /// Deposit counter after processing.
    pub next_deposit_number: u64,
    /// Enabled-token prefix at the start of the batch.
    pub prev_processed_token_count: u64,
    /// Enabled-token prefix after the batch.
    pub next_processed_token_count: u64,
    /// Withdrawal queue hash for this batch (`B256::ZERO` if no withdrawals).
    pub withdrawal_queue_hash: B256,
    /// L2 withdrawal batch index validated against the portal before submission.
    pub withdrawal_batch_index: u64,
}

/// Immutable Tempo anchor selected for one settlement attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchAnchor {
    /// The batch's Tempo block remains directly available through EIP-2935.
    Direct {
        /// Canonical hash of `BatchData::tempo_block_number` at preparation time.
        block_hash: B256,
    },
    /// A recent L1 block anchors a parent-linked header chain from the batch's Tempo block.
    Ancestry {
        /// Recent L1 block number whose hash the portal passes to the verifier.
        block_number: u64,
        /// Canonical hash of `block_number` at preparation time.
        block_hash: B256,
        /// Headers from `BatchData::tempo_block_number + 1` through `block_number`.
        ancestry_headers: Vec<Bytes>,
    },
}

impl From<ancestry::Ancestry> for BatchAnchor {
    fn from(ancestry: ancestry::Ancestry) -> Self {
        Self::Ancestry {
            block_number: ancestry.anchor.number,
            block_hash: ancestry.anchor.hash,
            ancestry_headers: ancestry.headers,
        }
    }
}

impl BatchAnchor {
    /// Returns the L1 block whose hash the portal passes to the verifier.
    pub const fn block_number(&self, tempo_block_number: u64) -> u64 {
        match self {
            Self::Direct { .. } => tempo_block_number,
            Self::Ancestry { block_number, .. } => *block_number,
        }
    }

    /// Returns the canonical hash of the selected anchor block.
    pub const fn block_hash(&self) -> B256 {
        match self {
            Self::Direct { block_hash } | Self::Ancestry { block_hash, .. } => *block_hash,
        }
    }

    /// Returns the prepared ancestry headers, empty in direct mode.
    pub fn ancestry_headers(&self) -> &[Bytes] {
        match self {
            Self::Direct { .. } => &[],
            Self::Ancestry {
                ancestry_headers, ..
            } => ancestry_headers,
        }
    }

    /// Returns the `recentTempoBlockNumber` argument for `submitBatch`.
    pub const fn recent_block_number(&self) -> u64 {
        match self {
            Self::Direct { .. } => 0,
            Self::Ancestry { block_number, .. } => *block_number,
        }
    }

    const fn mode_name(&self) -> &'static str {
        match self {
            Self::Direct { .. } => "direct",
            Self::Ancestry { .. } => "ancestry",
        }
    }
}

/// Batch commitments and their single authoritative Tempo anchor.
#[derive(Debug, Clone)]
pub struct PreparedBatch {
    pub batch: BatchData,
    pub anchor: BatchAnchor,
}

impl PreparedBatch {
    /// Returns the L1 block whose hash the portal passes to the verifier.
    pub const fn anchor_block_number(&self) -> u64 {
        self.anchor.block_number(self.batch.tempo_block_number)
    }
}

/// One L2 withdrawal batch finalized by `ZoneOutbox`.
#[derive(Debug, Clone)]
pub(crate) struct FinalizedBatch {
    /// Authoritative hash emitted by `BatchFinalized` and stored in `lastBatch()`.
    pub finalized_hash: B256,
    /// Authoritative L2 withdrawal batch index emitted by `BatchFinalized`.
    pub finalized_index: u64,
    /// Reconstructed withdrawal payloads for the off-chain processor store.
    pub withdrawals: Vec<abi::Withdrawal>,
}

#[derive(Debug, Clone)]
pub(crate) struct FinalizedBatchLog {
    pub(crate) block_number: u64,
    tx_index: u64,
    log_index: u64,
    tx_hash: B256,
    withdrawal_queue_hash: B256,
    withdrawal_batch_index: u64,
}

/// Zone L2 state read at a specific block, used to populate [`BatchData`].
pub(crate) struct ZoneBlockSnapshot {
    /// Latest Tempo L1 block number as seen by the zone.
    pub tempo_block_number: u64,
    /// Cumulative hash of all deposits processed by the zone up to this block.
    pub processed_deposit_hash: B256,
    /// Total number of deposits processed by the zone up to this block.
    pub processed_deposit_number: u64,
    /// Number of portal token enablements processed by the zone.
    pub processed_token_count: u64,
    /// Zone L2 block hash.
    pub block_hash: B256,
}

struct SettlementAttestationInput<'a> {
    batch: &'a BatchData,
    settlement_abi: SettlementAbi,
    anchor_block_number: u64,
    anchor_block_hash: B256,
    block_transition: &'a BlockTransition,
    deposit_transition: &'a DepositQueueTransition,
    verifier_config: &'a Bytes,
}

fn settlement_proof(
    verifier_mode: VerifierMode,
    proof_bundle: Option<&ProofBundle>,
) -> Result<(Bytes, Bytes)> {
    let config = Bytes::from_static(verifier_mode.config());
    let Some(bundle) = proof_bundle else {
        return Ok((config, Bytes::new()));
    };
    eyre::ensure!(
        VerifierMode::try_from(bundle.verifier_config.as_ref())? == verifier_mode,
        "proof bundle verifier mode does not match requested mode"
    );
    verifier_mode.validate_proof_shape(&bundle.proof)?;
    Ok((config, bundle.proof.clone()))
}

#[derive(Debug, Clone, Copy)]
struct StablePortalMetadata {
    zone_id: u32,
    chain_id: u64,
}

struct RawPortalSubmissionMetadata {
    withdrawal_batch_index: u64,
    sequencer_set_version: u64,
    sequencer_threshold: u8,
    signer_is_sequencer: bool,
    verifier: Address,
}

#[derive(Debug, Clone, Copy)]
struct PortalSubmissionMetadata {
    withdrawal_batch_index: u64,
    stable: StablePortalMetadata,
    sequencer_set_version: u64,
    sequencer_threshold: u8,
    signer_is_sequencer: bool,
    verifier: Address,
}
/// Pure function that resolves pre-fetched data into verified withdrawal sets
/// ready to be stored.
///
/// For each pending portal slot in `[head, tail)`:
/// 1. Skips slots with no `BatchSubmitted` event or no fetched withdrawals.
/// 2. Verifies the hash chain of fetched withdrawals matches the L1 event's
///    `withdrawalQueueHash`.
/// 3. For the head slot, trims already-processed withdrawals using
///    `head_slot_hash` (the current on-chain slot hash). The L1 portal
///    processes withdrawals one-by-one, updating the slot hash after each.
///    If the sequencer crashed mid-slot, some are already consumed but `head`
///    hasn't advanced yet.
/// 4. Non-head slots are always fully unprocessed.
///
/// Returns a map of portal_slot → verified withdrawals to store.
fn resolve_pending_slots(
    head: u64,
    tail: u64,
    events: &BTreeMap<u64, BatchSubmitted>,
    slot_withdrawals: &BTreeMap<u64, Vec<abi::Withdrawal>>,
    head_slot_hash: B256,
) -> Result<BTreeMap<u64, Vec<abi::Withdrawal>>> {
    if head < tail && head_slot_hash.is_zero() {
        eyre::bail!("pending withdrawal head slot {head} is zero for queue range {head}..{tail}");
    }

    let mut result: BTreeMap<u64, Vec<abi::Withdrawal>> = BTreeMap::new();

    for portal_slot in head..tail {
        let Some(event) = events.get(&portal_slot) else {
            eyre::bail!("no BatchSubmitted event found for pending portal slot {portal_slot}");
        };

        let Some(withdrawals) = slot_withdrawals.get(&portal_slot) else {
            eyre::bail!("no withdrawal data fetched for pending portal slot {portal_slot}");
        };

        if withdrawals.is_empty()
            || abi::Withdrawal::queue_hash(withdrawals) != event.withdrawalQueueHash
        {
            eyre::bail!("withdrawal hash mismatch or empty for portal slot {portal_slot}");
        }

        if portal_slot == head {
            match find_processed_offset(withdrawals, head_slot_hash) {
                Some(offset) => {
                    let remaining = withdrawals[offset..].to_vec();
                    if !remaining.is_empty() {
                        result.insert(portal_slot, remaining);
                    }
                }
                None => {
                    eyre::bail!("cannot determine processed offset for head slot {portal_slot}");
                }
            }
        } else {
            result.insert(portal_slot, withdrawals.clone());
        }
    }

    Ok(result)
}

/// Find the offset into `withdrawals` where the remaining hash chain matches
/// `current_slot_hash`. Returns `Some(0)` if no withdrawals have been processed,
/// `Some(n)` if n have been processed, or `None` if no match is found.
pub(crate) fn find_processed_offset(
    withdrawals: &[abi::Withdrawal],
    current_slot_hash: B256,
) -> Option<usize> {
    if current_slot_hash == B256::ZERO {
        return Some(withdrawals.len());
    }

    let mut hash = B256::ZERO;
    for (offset, withdrawal) in withdrawals.iter().enumerate().rev() {
        hash = withdrawal.hash_with_tail(hash);
        if hash == current_slot_hash {
            return Some(offset);
        }
    }
    None
}

fn block_with_receipts<P: ZoneSequencerProvider>(
    provider: &P,
    number: u64,
) -> Result<(Block, Vec<TempoReceipt>)> {
    let block = provider
        .block_by_number(number)?
        .ok_or_else(|| eyre::eyre!("canonical zone block {number} not found"))?;
    let receipts = provider
        .receipts_by_block(BlockHashOrNumber::Number(number))?
        .ok_or_else(|| eyre::eyre!("receipts for canonical zone block {number} not found"))?;
    if block.body.transactions.len() != receipts.len() {
        return Err(eyre::eyre!(
            "zone block {number} has {} transactions but {} receipts",
            block.body.transactions.len(),
            receipts.len()
        ));
    }
    Ok((block, receipts))
}

/// Read the settlement commitments emitted by the deterministic system transaction in a zone
/// block.
pub(crate) fn read_zone_block_snapshot<P: ZoneSequencerProvider>(
    provider: &P,
    inbox_address: Address,
    number: u64,
) -> Result<ZoneBlockSnapshot> {
    let (_, receipts) = block_with_receipts(provider, number)?;
    let block_hash = provider
        .block_hash(number)?
        .ok_or_else(|| eyre::eyre!("canonical zone block {number} is missing its hash"))?;
    let mut tempo_block_number = None;
    let mut processed_deposit_hash = None;
    let mut processed_deposit_number = None;
    let mut processed_token_count = None;

    for receipt in receipts {
        for log in receipt.logs() {
            if log.address != inbox_address {
                continue;
            }
            let Some(topic) = log.topics().first() else {
                continue;
            };
            let (block_number, deposit_hash, deposit_number, token_count) = match *topic {
                TempoAdvanced::SIGNATURE_HASH => {
                    let event = TempoAdvanced::decode_log(log).map_err(|err| {
                        eyre::eyre!("invalid post-T13 TempoAdvanced log in block {number}: {err}")
                    })?;
                    (
                        event.tempoBlockNumber,
                        event.newProcessedDepositQueueHash,
                        event.lastProcessedDepositNumber,
                        event.lastProcessedEnabledTokenCount,
                    )
                }
                LegacyTempoAdvanced::SIGNATURE_HASH => {
                    let event = LegacyTempoAdvanced::decode_log(log).map_err(|err| {
                        eyre::eyre!("invalid legacy TempoAdvanced log in block {number}: {err}")
                    })?;
                    (
                        event.tempoBlockNumber,
                        event.newProcessedDepositQueueHash,
                        event.lastProcessedDepositNumber,
                        0,
                    )
                }
                _ => {
                    continue;
                }
            };
            if tempo_block_number.replace(block_number).is_some() {
                return Err(eyre::eyre!(
                    "zone block {number} contains more than one TempoAdvanced event"
                ));
            }
            processed_deposit_hash = Some(deposit_hash);
            processed_deposit_number = Some(deposit_number);
            processed_token_count = Some(token_count);
        }
    }

    Ok(ZoneBlockSnapshot {
        tempo_block_number: tempo_block_number
            .ok_or_else(|| eyre::eyre!("zone block {number} is missing TempoAdvanced"))?,
        processed_deposit_hash: processed_deposit_hash
            .ok_or_else(|| eyre::eyre!("zone block {number} is missing its deposit commitment"))?,
        processed_deposit_number: processed_deposit_number
            .ok_or_else(|| eyre::eyre!("zone block {number} is missing its deposit number"))?,
        processed_token_count: processed_token_count
            .ok_or_else(|| eyre::eyre!("zone block {number} is missing its token cursor"))?,
        block_hash,
    })
}

/// Fetch all zone block numbers in `[from, to]` that finalized a withdrawal batch.
///
/// This includes zero-withdrawal batches because they still advance the L2
/// withdrawal batch index and therefore require a matching L1 `submitBatch`.
pub(crate) async fn fetch_finalized_batch_boundaries<P: ZoneSequencerProvider>(
    provider: &P,
    outbox_address: Address,
    from: u64,
    to: u64,
) -> Result<Vec<FinalizedBatchLog>> {
    if from > to {
        return Ok(Vec::new());
    }

    let boundaries = fetch_finalized_batch_logs(provider, outbox_address, from, to)?;
    if let Some(duplicate) = boundaries
        .windows(2)
        .find(|pair| pair[0].block_number == pair[1].block_number)
    {
        return Err(eyre::eyre!(
            "zone block {} contains more than one BatchFinalized event",
            duplicate[0].block_number
        ));
    }
    Ok(boundaries)
}

/// Fetch one finalized L2 withdrawal batch.
///
/// The submitted hash and index come from the supplied `BatchFinalized` event.
/// Withdrawal structs are reconstructed from `WithdrawalRequested` logs in the
/// same block: every non-empty withdrawal batch is finalized in the block that
/// contains its requests.
pub(crate) async fn fetch_finalized_batch<P: ZoneSequencerProvider>(
    zone_provider: &P,
    outbox_address: Address,
    target: &FinalizedBatchLog,
) -> Result<FinalizedBatch> {
    let (block, receipts) = block_with_receipts(zone_provider, target.block_number)?;
    let mut requests = Vec::new();
    for (tx, receipt) in block.body.transactions.iter().zip(&receipts) {
        for log in receipt.logs() {
            if log.address != outbox_address
                || log.topics().first() != Some(&IZoneOutbox::WithdrawalRequested::SIGNATURE_HASH)
            {
                continue;
            }
            let event = IZoneOutbox::WithdrawalRequested::decode_log(log)
                .map_err(|err| {
                    eyre::eyre!(
                        "invalid WithdrawalRequested log in zone block {}: {err}",
                        target.block_number
                    )
                })?
                .data;
            requests.push((*tx.tx_hash(), event));
        }
    }

    let finalize_tx = block
        .body
        .transactions
        .iter()
        .find(|tx| *tx.tx_hash() == target.tx_hash)
        .ok_or_else(|| {
            eyre::eyre!(
                "missing finalizeWithdrawalBatch tx {} for zone block {}",
                target.tx_hash,
                target.block_number
            )
        })?;
    let encrypted_senders =
        abi::IZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(finalize_tx.input().as_ref())
            .map_err(|err| {
                eyre::eyre!(
                    "failed to decode finalizeWithdrawalBatch calldata for {}: {err}",
                    target.tx_hash
                )
            })?
            .encryptedSenders;

    if encrypted_senders.len() != requests.len() {
        return Err(eyre::eyre!(
            "encrypted sender count mismatch for batch ending at zone block {}: {} encrypted senders for {} requests",
            target.block_number,
            encrypted_senders.len(),
            requests.len()
        ));
    }

    let withdrawals = requests
        .into_iter()
        .zip(encrypted_senders)
        .map(|((tx_hash, event), encrypted_sender)| {
            abi::Withdrawal::from_requested_event(&event, tx_hash, encrypted_sender)
        })
        .collect::<Vec<_>>();

    let recomputed_hash = abi::Withdrawal::queue_hash(&withdrawals);
    if recomputed_hash != target.withdrawal_queue_hash {
        return Err(eyre::eyre!(
            "withdrawal hash mismatch for batch ending at zone block {}: event hash {}, reconstructed hash {}",
            target.block_number,
            target.withdrawal_queue_hash,
            recomputed_hash
        ));
    }

    Ok(FinalizedBatch {
        finalized_hash: target.withdrawal_queue_hash,
        finalized_index: target.withdrawal_batch_index,
        withdrawals,
    })
}

/// Fetch `WithdrawalRequested` events for one portal queue slot.
///
/// A portal queue slot is created only for a non-empty withdrawal batch. The
/// outbox finalizes pending withdrawals in the same zone block as their
/// requests, so recovery needs to inspect only the block referenced by the
/// slot's `BatchSubmitted.nextBlockHash`.
pub(crate) async fn fetch_slot_withdrawals(
    zone_provider: &impl ZoneSequencerProvider,
    outbox_address: Address,
    block_number: u64,
) -> Result<Vec<abi::Withdrawal>> {
    let boundaries =
        fetch_finalized_batch_boundaries(zone_provider, outbox_address, block_number, block_number)
            .await?;
    let target = boundaries.into_iter().next().ok_or_else(|| {
        eyre::eyre!("zone block {block_number} does not contain a BatchFinalized boundary")
    })?;
    Ok(
        fetch_finalized_batch(zone_provider, outbox_address, &target)
            .await?
            .withdrawals,
    )
}

fn fetch_finalized_batch_logs<P: ZoneSequencerProvider>(
    provider: &P,
    outbox_address: Address,
    from: u64,
    to: u64,
) -> Result<Vec<FinalizedBatchLog>> {
    let mut finalized_batches = Vec::new();
    for block_number in from..=to {
        let (block, receipts) = block_with_receipts(provider, block_number)?;
        for (tx_index, (tx, receipt)) in block
            .body
            .transactions
            .iter()
            .zip(receipts.iter())
            .enumerate()
        {
            for (log_index, log) in receipt.logs().iter().enumerate() {
                if log.address != outbox_address
                    || log.topics().first() != Some(&IZoneOutbox::BatchFinalized::SIGNATURE_HASH)
                {
                    continue;
                }
                let event = IZoneOutbox::BatchFinalized::decode_log(log).map_err(|err| {
                    eyre::eyre!("invalid BatchFinalized log in zone block {block_number}: {err}")
                })?;
                finalized_batches.push(FinalizedBatchLog {
                    block_number,
                    tx_index: tx_index as u64,
                    log_index: log_index as u64,
                    tx_hash: *tx.tx_hash(),
                    withdrawal_queue_hash: event.withdrawalQueueHash,
                    withdrawal_batch_index: event.withdrawalBatchIndex,
                });
            }
        }
    }
    finalized_batches.sort_by_key(|batch| (batch.block_number, batch.tx_index, batch.log_index));
    Ok(finalized_batches)
}

fn backward_log_query_start(hi: u64, floor: u64) -> u64 {
    hi.saturating_sub(LOG_QUERY_BLOCK_CHUNK - 1).max(floor)
}

#[cfg(test)]
mod tests {
    use super::{
        ancestry::test_utils::{
            encoded_headers, mock_l1, mock_l1_chain, mock_l1_header, push_headers,
        },
        *,
    };
    use crate::abi::{self, legacySubmitBatchCall, submitBatchCall};
    use alloy_consensus::Header as ConsensusHeader;
    use alloy_primitives::{B256, address};
    use alloy_provider::ProviderBuilder;
    use alloy_rpc_types_eth::Header as RpcHeader;
    use alloy_sol_types::SolValue;
    use alloy_transport::mock::Asserter;
    use reth_provider::test_utils::MockEthProvider;
    use tempo_alloy::rpc::TempoHeaderResponse;
    use tempo_primitives::{Block, TempoHeader, TempoPrimitives};
    use zone_chainspec::test_utils::set_tempo_fork;

    fn test_chain_spec() -> Arc<ZoneChainSpec> {
        Arc::new(ZoneChainSpec {
            inner: tempo_chainspec::spec::DEV.clone(),
        })
    }

    fn chain_spec_with_t13(activation: u64) -> Arc<ZoneChainSpec> {
        let mut genesis = tempo_chainspec::spec::DEV.inner.genesis.clone();
        set_tempo_fork(&mut genesis, TempoHardfork::T13, activation);
        Arc::new(ZoneChainSpec {
            inner: Arc::new(tempo_chainspec::TempoChainSpec::from_genesis(genesis)),
        })
    }

    fn mock_fork_header(timestamp: u64) -> serde_json::Value {
        let header = TempoHeaderResponse {
            inner: RpcHeader {
                hash: B256::ZERO,
                inner: TempoHeader {
                    inner: ConsensusHeader {
                        timestamp,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                total_difficulty: None,
                size: None,
            },
            timestamp_millis: 0,
        };
        serde_json::to_value(header).unwrap()
    }

    #[test]
    fn settlement_bindings_keep_legacy_and_t13_selectors_distinct() {
        let legacy: [u8; 4] = keccak256(
            "submitBatch(uint64,uint64,(bytes32,bytes32),(bytes32,bytes32,uint64,uint64),bytes32,bytes,bytes,uint256,bytes[])"
        )[..4]
            .try_into()
            .unwrap();
        assert_eq!(legacySubmitBatchCall::SELECTOR, legacy);
        assert_ne!(legacySubmitBatchCall::SELECTOR, submitBatchCall::SELECTOR);
    }

    #[tokio::test]
    async fn settlement_abi_follows_live_l1_hardfork() {
        let chain_spec = chain_spec_with_t13(1_000);
        let legacy = Asserter::new();
        legacy.push_success(&mock_fork_header(999));
        assert_eq!(
            SettlementAbi::from_l1(&mock_l1(legacy), &chain_spec)
                .await
                .unwrap(),
            SettlementAbi::Legacy
        );

        let t13 = Asserter::new();
        t13.push_success(&mock_fork_header(1_000));
        assert_eq!(
            SettlementAbi::from_l1(&mock_l1(t13), &chain_spec)
                .await
                .unwrap(),
            SettlementAbi::T13
        );
    }

    #[tokio::test]
    async fn stale_prover_policy_is_rejected_before_submission() {
        // This also covers reorgs: a T13 proof cannot be sent while T12 is active again.
        for (proved, timestamp) in [(TempoHardfork::T12, 1_000), (TempoHardfork::T13, 999)] {
            let l1 = Asserter::new();
            l1.push_success(&mock_fork_header(timestamp));
            let proof = SettlementProof {
                bundle: ProofBundle {
                    verifier_config: Bytes::from_static(VerifierMode::NitroV1.config()),
                    proof: Bytes::new(),
                },
                hardfork: proved,
            };
            let submitter = BatchSubmitter::new(
                Address::repeat_byte(0x11),
                mock_l1(l1.clone()),
                chain_spec_with_t13(1_000),
            );
            let error = submitter
                .submit_batch(
                    &test_prepared_batch(120, 100),
                    Some(&proof),
                    None,
                    VerifierMode::NitroV1,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                BatchSubmitError::ProverHardforkChanged { .. }
            ));
            assert!(l1.read_q().is_empty());
        }
    }

    #[test]
    fn t13_uses_token_transition_for_legacy_boundary() {
        assert_eq!(
            SettlementAbi::Legacy.token_transition_hash(0, 0),
            B256::ZERO
        );
        assert_eq!(
            SettlementAbi::T13.token_transition_hash(0, 0),
            keccak256((0_u64, 0_u64).abi_encode())
        );
        assert_ne!(SettlementAbi::T13.token_transition_hash(0, 0), B256::ZERO);
    }

    fn test_prepared_batch(zone_height: u64, tempo_block_number: u64) -> PreparedBatch {
        PreparedBatch {
            batch: BatchData {
                zone_height,
                tempo_block_number,
                prev_block_hash: B256::repeat_byte(0x24),
                next_block_hash: B256::repeat_byte(0x25),
                prev_processed_deposit_hash: B256::ZERO,
                next_processed_deposit_hash: B256::ZERO,
                prev_deposit_number: 0,
                next_deposit_number: 0,
                prev_processed_token_count: 0,
                next_processed_token_count: 0,
                withdrawal_queue_hash: B256::ZERO,
                withdrawal_batch_index: 1,
            },
            anchor: BatchAnchor::Direct {
                block_hash: B256::repeat_byte(0x26),
            },
        }
    }

    #[tokio::test]
    async fn resolves_portal_hash_to_local_zone_height() {
        let portal_hash = B256::repeat_byte(0x42);
        let portal_height = 15_552_000;
        let zone = MockEthProvider::<TempoPrimitives>::new();
        let mut header = TempoHeader::default();
        header.inner.number = portal_height;
        zone.add_block(
            portal_hash,
            Block {
                header,
                body: Default::default(),
            },
        );

        let l1 = Asserter::new();
        l1.push_success(&abi_encode_multicall(vec![
            abi_word(portal_hash),
            abi_word(U256::from(portal_height)),
        ]));

        let anchor =
            resolve_portal_zone_anchor(&zone, Address::repeat_byte(0x11), &mock_l1(l1.clone()))
                .await
                .unwrap();

        assert_eq!(anchor.block_hash, portal_hash);
        assert_eq!(anchor.block_number, portal_height);
        assert!(l1.read_q().is_empty());
    }

    #[tokio::test]
    async fn zero_portal_hash_resolves_to_genesis() {
        let l1 = Asserter::new();
        l1.push_success(&abi_encode_multicall(vec![
            abi_word(B256::ZERO),
            abi_word(U256::ZERO),
        ]));

        let anchor = resolve_portal_zone_anchor(
            &MockEthProvider::<TempoPrimitives>::new(),
            Address::repeat_byte(0x11),
            &mock_l1(l1),
        )
        .await
        .unwrap();

        assert_eq!(anchor.block_hash, B256::ZERO);
        assert_eq!(anchor.block_number, 0);
    }

    #[tokio::test]
    async fn rejects_zero_portal_hash_at_nonzero_height() {
        for zone_height in [U256::from(1), U256::from(15_552_000), U256::MAX] {
            let l1 = Asserter::new();
            l1.push_success(&abi_encode_multicall(vec![
                abi_word(B256::ZERO),
                abi_word(zone_height),
            ]));

            let error = resolve_portal_zone_anchor(
                &MockEthProvider::<TempoPrimitives>::new(),
                Address::repeat_byte(0x11),
                &mock_l1(l1.clone()),
            )
            .await
            .unwrap_err();

            assert_eq!(
                error.to_string(),
                format!(
                    "inconsistent ZonePortal checkpoint: zero block hash at nonzero Zone height {zone_height}"
                )
            );
            assert!(l1.read_q().is_empty());
        }
    }

    #[tokio::test]
    async fn rejects_portal_hash_at_mismatched_height() {
        let portal_hash = B256::repeat_byte(0x42);
        let zone = MockEthProvider::<TempoPrimitives>::new();
        let mut header = TempoHeader::default();
        header.inner.number = 42;
        zone.add_block(
            portal_hash,
            Block {
                header,
                body: Default::default(),
            },
        );

        for zone_height in [U256::ZERO, U256::from(41), U256::from(43), U256::MAX] {
            let l1 = Asserter::new();
            l1.push_success(&abi_encode_multicall(vec![
                abi_word(portal_hash),
                abi_word(zone_height),
            ]));

            let error =
                resolve_portal_zone_anchor(&zone, Address::repeat_byte(0x11), &mock_l1(l1.clone()))
                    .await
                    .unwrap_err();

            assert_eq!(
                error.to_string(),
                format!(
                    "inconsistent ZonePortal checkpoint: block hash {portal_hash} is local Zone block 42, but portal Zone height is {zone_height}"
                )
            );
            assert!(l1.read_q().is_empty());
        }
    }

    #[tokio::test]
    async fn rejects_noncanonical_portal_hash() {
        let portal_hash = B256::repeat_byte(0x42);
        let l1 = Asserter::new();
        l1.push_success(&abi_encode_multicall(vec![
            abi_word(portal_hash),
            abi_word(U256::from(42)),
        ]));

        let err = resolve_portal_zone_anchor(
            &MockEthProvider::<TempoPrimitives>::new(),
            Address::repeat_byte(0x11),
            &mock_l1(l1),
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("is not canonical in the Zone node")
        );
    }

    fn abi_word(value: impl SolValue) -> Bytes {
        value.abi_encode().into()
    }

    fn abi_encode_multicall(values: Vec<Bytes>) -> Bytes {
        (U256::ZERO, values).abi_encode_params().into()
    }

    fn test_withdrawal(to: Address, amount: u128) -> abi::Withdrawal {
        abi::Withdrawal {
            token: address!("0x0000000000000000000000000000000000001000"),
            senderTag: B256::repeat_byte(0x11),
            to,
            amount,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackNonce: 1,
            callbackData: Default::default(),
            encryptedSender: Default::default(),
        }
    }

    #[test]
    fn batch_anchor_config_validates_effective_window() {
        let config = BatchAnchorConfig::new(10, 4).unwrap();
        assert_eq!(config.history_window(), 10);
        assert_eq!(config.safety_margin(), 4);
        assert_eq!(config.effective_window(), 6);

        assert!(BatchAnchorConfig::new(0, 0).is_err());
        assert!(BatchAnchorConfig::new(10, 10).is_err());
        assert!(BatchAnchorConfig::new(10, 11).is_err());
    }

    #[tokio::test]
    async fn anchor_resolution_accepts_observed_l1_tip() {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let submitter = BatchSubmitter::new(Address::ZERO, provider, test_chain_spec());

        let (header, hash) = mock_l1_header(100, B256::ZERO);
        asserter.push_success(&100_u64);
        asserter.push_success(&header);
        let anchor = submitter.resolve_batch_anchor(100).await.unwrap();

        assert_eq!(anchor.block_number(100), 100);
        assert_eq!(anchor.block_hash(), hash);
        assert!(anchor.ancestry_headers().is_empty());
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn anchor_resolution_rejects_future_l1_block() {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let submitter = BatchSubmitter::new(Address::ZERO, provider, test_chain_spec());

        asserter.push_success(&100_u64);
        let err = match submitter.resolve_batch_anchor(101).await {
            Ok(_) => panic!("future L1 anchor was accepted"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("tempo_block_number (101) is not yet confirmed on L1 (tip=100)")
        );
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn ancestry_anchor_uses_fresh_l1_hash() {
        let asserter = Asserter::new();
        let submitter = BatchSubmitter::with_anchor_config(
            Address::ZERO,
            mock_l1(asserter.clone()),
            test_chain_spec(),
            BatchAnchorConfig::new(10, 4).unwrap(),
        );
        let chain = mock_l1_chain(100, 106);

        // Gap 10 exceeds the effective window of 6, so the anchor sits at tip - margin.
        asserter.push_success(&110_u64);
        asserter.push_success(&chain[6]); // Fresh anchor read before loading ancestry.
        push_headers(&asserter, &chain);
        let anchor = submitter.resolve_batch_anchor(100).await.unwrap();

        assert_eq!(anchor.block_number(100), 106);
        assert_eq!(anchor.block_hash(), chain[6].inner.hash);
        assert_eq!(anchor.ancestry_headers(), encoded_headers(&chain[1..]));
        assert!(asserter.read_q().is_empty());

        // A cached range can become stale either at the anchor or when a new
        // suffix no longer links to it. Both must trigger a cache reset and retry.
        for new_suffix in [false, true] {
            submitter.ancestry.clear();
            push_headers(&asserter, &chain);
            submitter.ancestry.load(100, 106, |_| Ok(())).await.unwrap();

            let mut fork = chain.clone();
            fork[6].inner.inner.inner.timestamp += 1;
            fork[6].inner.hash = keccak256(alloy_rlp::encode(&fork[6].inner.inner));
            if new_suffix {
                fork.push(mock_l1_header(107, fork[6].inner.hash).0);
            }
            let expected = fork.last().unwrap();
            let anchor_number = 100 + fork.len() as u64 - 1;
            asserter.push_success(&(anchor_number + 4));
            asserter.push_success(expected);
            if new_suffix {
                asserter.push_success(expected); // First attempt cannot link the suffix.
            }
            push_headers(&asserter, &fork); // Retry must fetch the whole canonical range.

            let anchor = submitter.resolve_batch_anchor(100).await.unwrap();
            assert_eq!(anchor.block_hash(), expected.inner.hash);
            assert_eq!(anchor.block_number(100), anchor_number);
            assert_eq!(anchor.ancestry_headers(), encoded_headers(&fork[1..]));
            assert!(asserter.read_q().is_empty());
        }
    }

    #[tokio::test]
    async fn prepared_anchor_survives_forward_head_drift() {
        let asserter = Asserter::new();
        let provider = mock_l1(asserter.clone());
        let submitter = BatchSubmitter::new(Address::ZERO, provider, test_chain_spec());
        let (header, hash) = mock_l1_header(162_196, B256::ZERO);
        let mut prepared = test_prepared_batch(120, 160_000);
        prepared.anchor = BatchAnchor::Ancestry {
            block_number: 162_196,
            block_hash: hash,
            ancestry_headers: vec![Bytes::from_static(&[1])],
        };

        asserter.push_success(&162_208_u64);
        asserter.push_success(&header);
        let observed_head = submitter.validate_prepared_anchor(&prepared).await.unwrap();

        assert_eq!(observed_head, 162_208);
        assert_eq!(prepared.anchor_block_number(), 162_196);
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn prepared_anchor_hard_expiry_requires_a_new_attempt() {
        let asserter = Asserter::new();
        let provider = mock_l1(asserter.clone());
        let submitter = BatchSubmitter::with_anchor_config(
            Address::ZERO,
            provider,
            test_chain_spec(),
            BatchAnchorConfig::new(10, 4).unwrap(),
        );
        let mut prepared = test_prepared_batch(120, 90);
        prepared.anchor = BatchAnchor::Ancestry {
            block_number: 100,
            block_hash: B256::repeat_byte(0x26),
            ancestry_headers: vec![Bytes::from_static(&[1])],
        };

        asserter.push_success(&110_u64);
        let error = submitter
            .validate_prepared_anchor(&prepared)
            .await
            .unwrap_err();

        assert!(matches!(error, BatchSubmitError::PreparedAnchorInvalid(_)));
        assert!(
            error
                .to_string()
                .contains("outside the EIP-2935 history window")
        );
        assert!(asserter.read_q().is_empty());
    }

    #[test]
    fn certificate_must_commit_the_prepared_anchor_exactly() {
        let submitter =
            BatchSubmitter::new(Address::ZERO, mock_l1(Asserter::new()), test_chain_spec());
        let prepared = test_prepared_batch(120, 100);
        let batch = &prepared.batch;
        let metadata = PortalSubmissionMetadata {
            withdrawal_batch_index: 0,
            stable: StablePortalMetadata {
                zone_id: 7,
                chain_id: 1,
            },
            sequencer_set_version: 3,
            sequencer_threshold: 1,
            signer_is_sequencer: true,
            verifier: Address::repeat_byte(2),
        };
        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 3,
            zoneHeight: U256::from(batch.zone_height),
            withdrawalBatchIndex: U256::from(batch.withdrawal_batch_index),
            verifier: metadata.verifier,
            tempoBlockNumber: batch.tempo_block_number,
            anchorBlockNumber: prepared.anchor_block_number() + 12,
            anchorBlockHash: B256::repeat_byte(0x99),
            blockTransitionHash: keccak256(
                (batch.prev_block_hash, batch.next_block_hash).abi_encode(),
            ),
            depositQueueTransitionHash: keccak256(
                (
                    batch.prev_processed_deposit_hash,
                    batch.next_processed_deposit_hash,
                    batch.prev_deposit_number,
                    batch.next_deposit_number,
                )
                    .abi_encode(),
            ),
            tokenEnablementTransitionHash: B256::ZERO,
            withdrawalQueueHash: batch.withdrawal_queue_hash,
            verifierConfigHash: VerifierMode::NitroV1.config_hash(),
        };
        let certificate = SettlementCertificate {
            height: batch.zone_height,
            digest: B256::ZERO,
            attestation,
            signatures: Vec::new(),
        };

        let error = submitter
            .validate_certificate(
                &prepared,
                SettlementAbi::Legacy,
                batch.zone_height,
                metadata,
                VerifierMode::NitroV1,
                &certificate,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("certificate anchor block changed")
        );
    }

    #[test]
    fn settlement_proof_enforces_verifier_mode_shape() {
        let (verifier_config, proof) = settlement_proof(VerifierMode::NoProof, None).unwrap();
        assert_eq!(verifier_config.as_ref(), &[2]);
        assert!(proof.is_empty());

        let empty_nitro = ProofBundle {
            verifier_config: Bytes::from_static(VerifierMode::NitroV1.config()),
            proof: Bytes::new(),
        };
        assert!(settlement_proof(VerifierMode::NitroV1, Some(&empty_nitro)).is_err());

        let mismatched = ProofBundle {
            verifier_config: Bytes::from_static(VerifierMode::NoProof.config()),
            proof: Bytes::new(),
        };
        assert!(settlement_proof(VerifierMode::NitroV1, Some(&mismatched)).is_err());
        assert!(settlement_proof(VerifierMode::NoProof, Some(&mismatched)).is_ok());

        let nonempty_no_proof = ProofBundle {
            verifier_config: Bytes::from_static(VerifierMode::NoProof.config()),
            proof: Bytes::from_static(&[0xaa]),
        };
        assert!(settlement_proof(VerifierMode::NoProof, Some(&nonempty_no_proof)).is_err());

        let bundle = ProofBundle {
            verifier_config: Bytes::from_static(VerifierMode::NitroV1.config()),
            proof: Bytes::from_static(&[0xaa, 0xbb]),
        };

        let (verifier_config, proof) =
            settlement_proof(VerifierMode::NitroV1, Some(&bundle)).unwrap();

        assert_eq!(verifier_config.as_ref(), VerifierMode::NitroV1.config());
        assert_eq!(proof.as_ref(), [0xaa, 0xbb]);
    }

    #[tokio::test]
    async fn submission_metadata_multicalls_mutable_values_and_caches_stable_values() {
        let asserter = Asserter::new();
        let portal_address = Address::repeat_byte(0x22);
        let signer = Address::repeat_byte(0x33);
        let verifier = Address::repeat_byte(0x44);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let submitter = BatchSubmitter::new(portal_address, provider, test_chain_spec());

        asserter.push_success(&abi_encode_multicall(vec![
            abi_word(7_u64),
            abi_word(11_u64),
            abi_word(U256::from(1)),
            abi_word(true),
            abi_word(verifier),
            abi_word(42_u32),
            abi_word(U256::from(42431)),
        ]));
        let first = submitter.read_submission_metadata(signer).await.unwrap();
        assert_eq!(first.withdrawal_batch_index, 7);
        assert_eq!(first.sequencer_set_version, 11);
        assert!(first.signer_is_sequencer);
        assert_eq!(first.verifier, verifier);
        assert_eq!(first.stable.zone_id, 42);
        assert_eq!(first.stable.chain_id, 42431);

        let next_verifier = Address::repeat_byte(0x55);
        asserter.push_success(&abi_encode_multicall(vec![
            abi_word(8_u64),
            abi_word(12_u64),
            abi_word(U256::from(1)),
            abi_word(false),
            abi_word(next_verifier),
        ]));
        let second = submitter.read_submission_metadata(signer).await.unwrap();
        assert_eq!(second.withdrawal_batch_index, 8);
        assert_eq!(second.sequencer_set_version, 12);
        assert!(!second.signer_is_sequencer);
        assert_eq!(second.verifier, next_verifier);
        assert_eq!(second.stable.zone_id, 42);
        assert_eq!(second.stable.chain_id, 42431);
        assert!(asserter.read_q().is_empty());
    }

    #[test]
    fn submission_metadata_requires_one_of_one_without_certificate() {
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(Asserter::new())
            .erased();
        let submitter = BatchSubmitter::new(Address::ZERO, provider, test_chain_spec());
        let batch = BatchData {
            zone_height: 1,
            tempo_block_number: 1,
            prev_block_hash: B256::ZERO,
            next_block_hash: B256::repeat_byte(0x11),
            prev_processed_deposit_hash: B256::ZERO,
            next_processed_deposit_hash: B256::ZERO,
            prev_deposit_number: 0,
            next_deposit_number: 0,
            prev_processed_token_count: 0,
            next_processed_token_count: 0,
            withdrawal_queue_hash: B256::ZERO,
            withdrawal_batch_index: 1,
        };
        let metadata = PortalSubmissionMetadata {
            withdrawal_batch_index: 0,
            stable: StablePortalMetadata {
                zone_id: 1,
                chain_id: 1,
            },
            sequencer_set_version: 1,
            sequencer_threshold: 2,
            signer_is_sequencer: true,
            verifier: Address::ZERO,
        };

        let err = submitter
            .validate_submission_metadata(&batch, metadata, false)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("supports only a 1-of-1 sequencer set")
        );

        submitter
            .validate_submission_metadata(&batch, metadata, true)
            .unwrap();
    }

    #[tokio::test]
    async fn direct_anchor_resolution_drops_ancestry_cache() {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let submitter = BatchSubmitter::new(Address::ZERO, provider, test_chain_spec());

        push_headers(&asserter, &mock_l1_chain(97, 98));
        submitter.ancestry.load(97, 98, |_| Ok(())).await.unwrap();
        assert!(!submitter.ancestry.is_empty());

        let (header, hash) = mock_l1_header(99, B256::ZERO);
        asserter.push_success(&100_u64);
        asserter.push_success(&header);
        let anchor = submitter.resolve_batch_anchor(99).await.unwrap();

        assert_eq!(anchor.block_hash(), hash);
        assert!(submitter.ancestry.is_empty());
        assert!(asserter.read_q().is_empty());
    }

    #[test]
    fn finds_processed_withdrawal_offset() {
        let withdrawals = vec![
            test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100),
            test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200),
            test_withdrawal(address!("0x0000000000000000000000000000000000000003"), 300),
        ];
        let cases = [
            (
                "full queue",
                abi::Withdrawal::queue_hash(&withdrawals),
                Some(0),
            ),
            (
                "partial queue",
                abi::Withdrawal::queue_hash(&withdrawals[1..]),
                Some(1),
            ),
            (
                "partial queue suffix",
                abi::Withdrawal::queue_hash(&withdrawals[2..]),
                Some(2),
            ),
            ("fully consumed", B256::ZERO, Some(withdrawals.len())),
            ("corrupted hash", B256::repeat_byte(0xde), None),
        ];

        for (case, current_slot_hash, expected) in cases {
            assert_eq!(
                find_processed_offset(&withdrawals, current_slot_hash),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn finds_processed_offset_for_empty_queue() {
        assert_eq!(find_processed_offset(&[], B256::ZERO), Some(0));
        assert_eq!(find_processed_offset(&[], B256::random()), None);
    }

    #[test]
    fn backward_log_query_window_is_bounded() {
        let hi = 10_000;
        let lo = backward_log_query_start(hi, 0);
        assert_eq!(lo, hi - LOG_QUERY_BLOCK_CHUNK + 1);
        assert_eq!(hi - lo + 1, LOG_QUERY_BLOCK_CHUNK);
        assert_eq!(backward_log_query_start(100, 50), 50);
    }

    fn test_batch_event(withdrawal_queue_hash: B256) -> BatchSubmitted {
        BatchSubmitted {
            withdrawalBatchIndex: 0,
            withdrawalQueueIndex: U256::ZERO,
            nextProcessedDepositQueueHash: B256::ZERO,
            nextBlockHash: B256::ZERO,
            withdrawalQueueHash: withdrawal_queue_hash,
            lastProcessedDepositNumber: 0,
            lastProcessedEnabledTokenCount: 0,
        }
    }

    #[test]
    fn decode_batch_submitted_from_receipt_logs() {
        use alloy_provider::ProviderBuilder;
        use alloy_transport::mock::Asserter;

        let portal_address = address!("0x7069DeC4E64Fd07334A0933eDe836C17259c9B23");
        let provider = ProviderBuilder::new_with_network::<tempo_alloy::TempoNetwork>()
            .connect_mocked_client(Asserter::new())
            .erased();
        let submitter = BatchSubmitter::new(portal_address, provider, test_chain_spec());

        let event = BatchSubmitted {
            withdrawalBatchIndex: 7,
            withdrawalQueueIndex: U256::from(3),
            nextProcessedDepositQueueHash: B256::repeat_byte(0x11),
            nextBlockHash: B256::repeat_byte(0x22),
            withdrawalQueueHash: B256::repeat_byte(0x33),
            lastProcessedDepositNumber: 9,
            lastProcessedEnabledTokenCount: 5,
        };
        let log = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log {
                address: portal_address,
                data: event.encode_log_data(),
            },
            ..Default::default()
        };
        let unrelated = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log {
                address: Address::repeat_byte(0x99),
                data: event.encode_log_data(),
            },
            ..Default::default()
        };

        let decoded = submitter
            .decode_batch_submitted(&[unrelated.clone(), log])
            .unwrap();
        assert_eq!(decoded.withdrawalBatchIndex, 7);
        assert_eq!(decoded.withdrawalQueueIndex, U256::from(3));
        assert_eq!(decoded.nextBlockHash, B256::repeat_byte(0x22));
        assert_eq!(decoded.lastProcessedEnabledTokenCount, 5);

        let legacy = abi::LegacyBatchSubmitted {
            withdrawalBatchIndex: 6,
            withdrawalQueueIndex: U256::from(2),
            nextProcessedDepositQueueHash: B256::repeat_byte(0x44),
            nextBlockHash: B256::repeat_byte(0x55),
            withdrawalQueueHash: B256::repeat_byte(0x66),
            lastProcessedDepositNumber: 8,
        };
        let legacy_log = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log {
                address: portal_address,
                data: legacy.encode_log_data(),
            },
            ..Default::default()
        };
        let decoded = submitter.decode_batch_submitted(&[legacy_log]).unwrap();
        assert_eq!(decoded.withdrawalBatchIndex, 6);
        assert_eq!(decoded.lastProcessedEnabledTokenCount, 0);

        assert!(submitter.decode_batch_submitted(&[unrelated]).is_err());
    }

    #[tokio::test]
    async fn finds_batch_events_by_logical_index_beyond_legacy_capacity() {
        use alloy_provider::ProviderBuilder;
        use alloy_transport::mock::Asserter;

        let portal_address = address!("0x7069DeC4E64Fd07334A0933eDe836C17259c9B23");
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new_with_network::<tempo_alloy::TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let submitter = BatchSubmitter::new(portal_address, provider, test_chain_spec());

        asserter.push_success(&10_000_u64);
        let logs: Vec<_> = [99_u64, 100, 101]
            .into_iter()
            .map(|index| {
                let event = BatchSubmitted {
                    withdrawalBatchIndex: index + 20,
                    withdrawalQueueIndex: U256::from(index),
                    nextProcessedDepositQueueHash: B256::ZERO,
                    nextBlockHash: B256::from(U256::from(index + 1)),
                    withdrawalQueueHash: B256::from(U256::from(index + 2)),
                    lastProcessedDepositNumber: 0,
                    lastProcessedEnabledTokenCount: 0,
                };
                alloy_rpc_types_eth::Log {
                    inner: alloy_primitives::Log {
                        address: portal_address,
                        data: event.encode_log_data(),
                    },
                    block_number: Some(9_900 + index),
                    ..Default::default()
                }
            })
            .collect();
        asserter.push_success(&logs);

        let events = submitter.find_batch_events_by_index(99, 102).await.unwrap();

        assert_eq!(
            events.keys().copied().collect::<Vec<_>>(),
            vec![99, 100, 101]
        );
        assert_eq!(events[&99].withdrawalBatchIndex, 119);
        assert_eq!(events[&100].withdrawalQueueIndex, U256::from(100));
        assert_eq!(events[&101].withdrawalQueueIndex, U256::from(101));
        assert!(asserter.read_q().is_empty());
    }

    #[test]
    fn resolve_empty_range() {
        let result =
            resolve_pending_slots(5, 5, &BTreeMap::new(), &BTreeMap::new(), B256::ZERO).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_single_slot_unprocessed() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let withdrawals = vec![w0, w1];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(full_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, full_hash).unwrap();
        let returned = result.get(&5).unwrap();
        assert_eq!(returned.len(), 2);
        assert_eq!(abi::Withdrawal::queue_hash(returned), full_hash);
    }

    #[test]
    fn resolve_single_slot_partially_processed() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000003"), 300);
        let withdrawals = vec![w0, w1, w2];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);
        // head_slot_hash reflects that w0 has been processed (hash of remaining [w1, w2])
        let head_slot_hash = abi::Withdrawal::queue_hash(&withdrawals[1..]);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(full_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let result =
            resolve_pending_slots(5, 6, &events, &slot_withdrawals, head_slot_hash).unwrap();
        let returned = result.get(&5).unwrap();
        assert_eq!(returned.len(), 2);
        assert_eq!(abi::Withdrawal::queue_hash(returned), head_slot_hash);
    }

    #[test]
    fn resolve_single_pending_slot_rejects_zero_hash() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w0];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(full_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let error =
            resolve_pending_slots(5, 6, &events, &slot_withdrawals, B256::ZERO).unwrap_err();
        assert!(error.to_string().contains("head slot 5 is zero"));
    }

    #[test]
    fn resolve_multiple_slots() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000003"), 300);

        let head_withdrawals = vec![w0];
        let tail_withdrawals = vec![w1, w2];

        let head_hash = abi::Withdrawal::queue_hash(&head_withdrawals);
        let tail_hash = abi::Withdrawal::queue_hash(&tail_withdrawals);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(head_hash));
        events.insert(6, test_batch_event(tail_hash));

        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, head_withdrawals);
        slot_withdrawals.insert(6, tail_withdrawals);

        // head slot fully unprocessed (head_slot_hash == full hash of slot 5)
        let result = resolve_pending_slots(5, 7, &events, &slot_withdrawals, head_hash).unwrap();
        let slot5 = result.get(&5).unwrap();
        let slot6 = result.get(&6).unwrap();
        assert_eq!(slot5.len(), 1);
        assert_eq!(slot6.len(), 2);
        assert_eq!(abi::Withdrawal::queue_hash(slot5), head_hash);
        assert_eq!(abi::Withdrawal::queue_hash(slot6), tail_hash);
    }

    #[test]
    fn resolve_hash_mismatch_skipped() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w0];
        let wrong_hash = B256::from([0xabu8; 32]);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(wrong_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, B256::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_missing_event_skipped() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w0];

        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        // No event for slot 5
        let result = resolve_pending_slots(5, 6, &BTreeMap::new(), &slot_withdrawals, B256::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_head_partial_with_non_head_slot() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000003"), 300);

        let head_withdrawals = vec![w0, w1];
        let non_head_withdrawals = vec![w2];

        let head_hash = abi::Withdrawal::queue_hash(&head_withdrawals);
        let non_head_hash = abi::Withdrawal::queue_hash(&non_head_withdrawals);
        // w0 already processed, head_slot_hash = hash of [w1] only
        let head_slot_hash = abi::Withdrawal::queue_hash(&head_withdrawals[1..]);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(head_hash));
        events.insert(6, test_batch_event(non_head_hash));

        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, head_withdrawals);
        slot_withdrawals.insert(6, non_head_withdrawals);

        let result =
            resolve_pending_slots(5, 7, &events, &slot_withdrawals, head_slot_hash).unwrap();
        // Head slot trimmed to 1 remaining withdrawal
        assert_eq!(result.get(&5).unwrap().len(), 1);
        assert_eq!(
            abi::Withdrawal::queue_hash(result.get(&5).unwrap()),
            head_slot_hash
        );
        // Non-head slot fully present
        assert_eq!(result.get(&6).unwrap().len(), 1);
        assert_eq!(
            abi::Withdrawal::queue_hash(result.get(&6).unwrap()),
            non_head_hash
        );
    }

    #[test]
    fn resolve_empty_withdrawals_vec_skipped() {
        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(B256::from([0x11u8; 32])));

        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, vec![]);

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, B256::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_missing_withdrawals_data_skipped() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let hash = abi::Withdrawal::queue_hash(std::slice::from_ref(&w));

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(hash));
        // slot_withdrawals has no entry for slot 5
        let slot_withdrawals = BTreeMap::new();

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, hash);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_head_slot_corrupted_hash_skipped() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);
        // head_slot_hash doesn't match any tail of the withdrawal list
        let corrupted_hash = B256::from([0xdeu8; 32]);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(full_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, corrupted_hash);
        assert!(result.is_err());
    }
}
