//! Batch-boundary settlement attestation construction and validation.

use std::{collections::HashMap, sync::Arc};

use alloy_consensus::{BlockHeader as _, TxReceipt as _};
use alloy_eips::{BlockHashOrNumber, BlockNumberOrTag};
use alloy_primitives::{B256, Sealable as _, U256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_rpc_types_eth::BlockId;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolEvent as _, SolValue as _};
use eyre::{OptionExt as _, WrapErr as _};
use reth_provider::HeaderProvider;
use reth_storage_api::{ReceiptProvider, StateProvider as _, StateProviderFactory};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::TempoHardforks as _;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::{
    IZoneOutbox, LegacyTempoAdvanced, TempoAdvanced, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
    ZonePortal,
};
use tracing::info;
use zone_chainspec::ZoneChainSpec;
use zone_l1::TempoStateExt as _;
use zone_prover::VerifierMode;

use zone_sequencer::{
    BatchAnchorConfig, SettlementAbi,
    attestation::{AttestationDomain, SettlementAttestation, SignedSettlementAttestation},
};

pub(crate) enum ProposedVerifierConfig {
    Hash(B256),
}

impl From<B256> for ProposedVerifierConfig {
    fn from(value: B256) -> Self {
        Self::Hash(value)
    }
}

impl From<VerifierMode> for ProposedVerifierConfig {
    fn from(value: VerifierMode) -> Self {
        Self::Hash(value.config_hash())
    }
}

/// Validate a fast settlement proposal against this replica's canonical state and durable Raft
/// head, then return a signature only to the authenticated proposing peer.
pub(crate) async fn sign_fast_settlement_proposal<P>(
    provider: &P,
    context: &AttestationContext,
    leader: zone_p2p::P2pPeerId,
    proposal: Vec<u8>,
    commands: &tokio::sync::mpsc::Sender<zone_p2p::P2pCommand>,
) -> eyre::Result<()>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider + StateProviderFactory,
{
    let proposal = SettlementAttestation::decode(&proposal)?;
    let height: u64 = proposal
        .zoneHeight
        .try_into()
        .wrap_err("settlement height does not fit in u64")?;
    let expected = build_settlement_attestation(
        provider,
        height,
        context,
        (proposal.anchorBlockNumber, proposal.anchorBlockHash),
        proposal.verifierConfigHash,
    )
    .await?
    .ok_or_eyre("proposed block is not a batch boundary")?;
    eyre::ensure!(
        proposal == expected,
        "settlement proposal does not match canonical replica state"
    );
    let signer = context
        .signer
        .as_ref()
        .ok_or_eyre("fast replica has no settlement signing key")?;
    let signed = SignedSettlementAttestation::sign(proposal, context.domain, signer)?;
    commands
        .send(zone_p2p::P2pCommand::SendSettlementSignature {
            leader,
            signature: signed.encode(),
        })
        .await
        .wrap_err("P2P command channel closed")
}

/// Shared signing and L1-validation context for settlement attestations.
#[derive(Clone)]
pub(crate) struct AttestationContext {
    pub(crate) domain: AttestationDomain,
    /// Portal sequencer-set version validated against the manifest at startup.
    pub(crate) pinned_sequencer_set_version: Option<u64>,
    /// `None` on an rpc-only member: it holds no individual key and never signs.
    pub(crate) signer: Option<PrivateKeySigner>,
    pub(crate) addresses: HashMap<zone_p2p::P2pPeerId, alloy_primitives::Address>,
    pub(crate) l1_provider: DynProvider<TempoNetwork>,
    pub(crate) chain_spec: Arc<ZoneChainSpec>,
    pub(crate) anchor_config: BatchAnchorConfig,
    /// Runtime-owned guard over the durable Raft-applied prefix. Fast settlement refuses to sign
    /// until the runtime installs this guard; a Reth persisted head is not a commitment oracle.
    committed_guard: Option<SettlementCommittedGuard>,
}

/// Runtime hook that proves a proposed batch endpoint is the exact canonical Raft committed head.
pub(crate) type SettlementCommittedGuard = Arc<dyn Fn(u64, B256) -> eyre::Result<()> + Send + Sync>;

impl AttestationContext {
    pub(crate) fn new(
        domain: AttestationDomain,
        pinned_sequencer_set_version: Option<u64>,
        signer: Option<PrivateKeySigner>,
        addresses: HashMap<zone_p2p::P2pPeerId, alloy_primitives::Address>,
        l1_provider: DynProvider<TempoNetwork>,
        chain_spec: Arc<ZoneChainSpec>,
        anchor_config: BatchAnchorConfig,
    ) -> Self {
        Self {
            domain,
            pinned_sequencer_set_version,
            signer,
            addresses,
            l1_provider,
            chain_spec,
            anchor_config,
            committed_guard: None,
        }
    }

    /// Install the runtime-owned durable committed-prefix guard used by T14 settlement signing.
    pub(crate) fn with_committed_guard(mut self, guard: SettlementCommittedGuard) -> Self {
        self.committed_guard = Some(guard);
        self
    }
}

/// Check the manifest's settlement quorum against `ZonePortal` before any role task starts.
///
/// A quorum node the portal has not registered can never settle, and an unreachable threshold
/// stalls settlement — both otherwise surface as a mysterious stall at the next batch boundary.
///
/// Extra registered signers only warn: deregistering is a cleanup task, and failing on it would
/// make every membership change a window in which no node can start.
///
/// A configured portal must already be deployed at the current L1 tip. The persisted Zone genesis
/// anchor may still predate portal deployment so the creation block can be replayed; this live-tip
/// check is deliberately independent of that historical anchor.
pub(crate) async fn validate_registered_sequencer_set(
    manifest: &zone_p2p::ZoneManifest,
    portal_address: alloy_primitives::Address,
    l1_provider: &alloy_provider::DynProvider<tempo_alloy::TempoNetwork>,
) -> eyre::Result<Option<u64>> {
    // Programmatic synthetic test harnesses use zero to mean no Portal; the production CLI
    // rejects a zero address before node launch.
    if portal_address.is_zero() {
        info!(target: "zone::p2p", "No ZonePortal configured; skipping the manifest quorum check");
        return Ok(None);
    }
    let portal_code = l1_provider
        .get_code_at(portal_address)
        .await
        .map_err(|err| eyre::eyre!("failed to check portal {portal_address} deployment: {err}"))?;
    eyre::ensure!(
        !portal_code.is_empty(),
        "ZonePortal {portal_address} is not deployed at the current L1 tip; refusing to start P2P before its sequencer set can be validated"
    );

    // Read at the chain tip rather than the finalized head: a fresh or local L1 may have no
    // finalized block at all, and an unfinalized registration satisfying this check early is
    // harmless for a startup sanity check.
    let portal = ZonePortal::new(portal_address, l1_provider);
    let quorum: Vec<_> = manifest.quorum_nodes().collect();
    let (sequencer_set_version, threshold, registered_count) = l1_provider
        .multicall()
        .add(portal.sequencerSetVersion())
        .add(portal.sequencerThreshold())
        .add(portal.sequencerCount())
        .aggregate()
        .await
        .wrap_err("failed reading the registered sequencer set from ZonePortal")?;
    let mut registration_calls = l1_provider.multicall().dynamic();
    for (_, address) in &quorum {
        registration_calls = registration_calls.add_dynamic(portal.isSequencer(*address));
    }
    let registered = registration_calls
        .aggregate()
        .await
        .wrap_err("failed reading the registered sequencers from ZonePortal")?;
    let validated_version = portal
        .sequencerSetVersion()
        .call()
        .await
        .wrap_err("failed re-reading the ZonePortal sequencer-set version")?;
    eyre::ensure!(
        validated_version == sequencer_set_version,
        "ZonePortal sequencer set changed during startup validation ({sequencer_set_version} -> {validated_version})"
    );

    for ((node, address), is_registered) in quorum.iter().zip(registered) {
        let name = node.name();
        eyre::ensure!(
            is_registered,
            "manifest quorum node `{name}` ({address}) is not a registered ZonePortal sequencer"
        );
    }
    eyre::ensure!(
        threshold > 0 && usize::from(threshold) <= quorum.len(),
        "ZonePortal settlement threshold {threshold} is not reachable by the manifest's {} quorum nodes",
        quorum.len()
    );
    if registered_count > U256::from(quorum.len()) {
        tracing::warn!(
            target: "zone::p2p",
            %registered_count,
            manifest_quorum = quorum.len(),
            "ZonePortal has registered sequencers the manifest does not list; they hold a share of the threshold this zone never signs for"
        );
    }

    info!(target: "zone::p2p", threshold, sequencer_set_version, quorum_nodes = quorum.len(), "Checked the manifest quorum against ZonePortal");
    Ok(Some(sequencer_set_version))
}

#[derive(Debug, Clone, Copy)]
struct BlockCommitments {
    tempo_block_hash: B256,
    tempo_block_number: u64,
    processed_deposit_hash: B256,
    processed_deposit_number: u64,
    processed_token_count: u64,
    withdrawal: Option<(B256, u64)>,
}

/// Extract commitments produced by the deterministic system transactions in a zone block.
fn block_commitments<P>(provider: &P, number: u64) -> eyre::Result<Option<BlockCommitments>>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider + StateProviderFactory,
{
    let receipts = provider
        .receipts_by_block(BlockHashOrNumber::Number(number))?
        .ok_or_eyre(format!(
            "receipts for canonical block {number} are not persisted"
        ))?;
    let mut anchor_hash = None;
    let mut tempo_block_number = None;
    let mut processed_deposit_hash = None;
    let mut processed_deposit_number = None;
    let mut processed_token_count = None;
    let mut withdrawal = None;

    for receipt in receipts {
        for log in receipt.logs() {
            if log.address == ZONE_INBOX_ADDRESS {
                match log.topics().first().copied() {
                    Some(TempoAdvanced::SIGNATURE_HASH) => {
                        let event = TempoAdvanced::decode_log(log).wrap_err_with(|| {
                            format!("invalid post-T13 TempoAdvanced log in block {number}")
                        })?;
                        anchor_hash = Some(event.tempoBlockHash);
                        tempo_block_number = Some(event.tempoBlockNumber);
                        processed_deposit_hash = Some(event.newProcessedDepositQueueHash);
                        processed_deposit_number = Some(event.lastProcessedDepositNumber);
                        processed_token_count = Some(event.lastProcessedEnabledTokenCount);
                    }
                    Some(LegacyTempoAdvanced::SIGNATURE_HASH) => {
                        let event = LegacyTempoAdvanced::decode_log(log).wrap_err_with(|| {
                            format!("invalid legacy TempoAdvanced log in block {number}")
                        })?;
                        anchor_hash = Some(event.tempoBlockHash);
                        tempo_block_number = Some(event.tempoBlockNumber);
                        processed_deposit_hash = Some(event.newProcessedDepositQueueHash);
                        processed_deposit_number = Some(event.lastProcessedDepositNumber);
                        processed_token_count = Some(0);
                    }
                    _ => {}
                }
            } else if log.address == ZONE_OUTBOX_ADDRESS
                && log.topics().first() == Some(&IZoneOutbox::BatchFinalized::SIGNATURE_HASH)
            {
                let event = IZoneOutbox::BatchFinalized::decode_log(log)
                    .wrap_err_with(|| format!("invalid BatchFinalized log in block {number}"))?;
                withdrawal = Some((event.withdrawalQueueHash, event.withdrawalBatchIndex));
            }
        }
    }

    // Checkpoint-only blocks have no BatchFinalized or TempoAdvanced event. They are not
    // settlement boundaries and must remain transparent to the previous-boundary scan.
    let Some(withdrawal) = withdrawal else {
        return Ok(None);
    };
    if let (
        Some(tempo_block_hash),
        Some(tempo_block_number),
        Some(processed_deposit_hash),
        Some(processed_deposit_number),
        Some(processed_token_count),
    ) = (
        anchor_hash,
        tempo_block_number,
        processed_deposit_hash,
        processed_deposit_number,
        processed_token_count,
    ) {
        return Ok(Some(BlockCommitments {
            tempo_block_hash,
            tempo_block_number,
            processed_deposit_hash,
            processed_deposit_number,
            processed_token_count,
            withdrawal: Some(withdrawal),
        }));
    }

    // A same-anchor boundary may contain BatchFinalized without TempoAdvanced. In that case the
    // exact canonical boundary state is the only authority for the retained import/cursors.
    let block_hash = provider
        .sealed_header(number)?
        .map(|header| header.hash())
        .ok_or_eyre(format!("missing canonical batch-boundary header {number}"))?;
    let state = provider.state_by_block_hash(block_hash)?;
    let imported_tempo = state.tempo_num_hash()?;
    let state_processed_deposit_hash = state
        .storage(
            ZONE_INBOX_ADDRESS,
            zone_precompiles::inbox::slots::PROCESSED_DEPOSIT_QUEUE_HASH.into(),
        )?
        .map(B256::from)
        .unwrap_or_default();
    let state_processed_deposit_number = state
        .storage(
            ZONE_INBOX_ADDRESS,
            zone_precompiles::inbox::slots::PROCESSED_DEPOSIT_NUMBER.into(),
        )?
        .unwrap_or_default()
        .to::<u64>();
    let state_processed_token_count = state
        .storage(
            ZONE_INBOX_ADDRESS,
            zone_precompiles::inbox::slots::PROCESSED_ENABLED_TOKEN_COUNT.into(),
        )?
        .unwrap_or_default()
        .to::<u64>();
    Ok(Some(BlockCommitments {
        tempo_block_hash: imported_tempo.hash,
        tempo_block_number: imported_tempo.number,
        processed_deposit_hash: state_processed_deposit_hash,
        processed_deposit_number: state_processed_deposit_number,
        processed_token_count: state_processed_token_count,
        withdrawal: Some(withdrawal),
    }))
}

/// Get the previous batch's (i.e the last block in the previous batch) block_hash,
/// deposit_hash, processed_deposit_number, processed_token_count, and withdrawal batch index.
/// These values identify the transition independently of L1 submission progress.
fn previous_batch<P>(provider: &P, number: u64) -> eyre::Result<(u64, B256, B256, u64, u64, u64)>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider + StateProviderFactory,
{
    for candidate in (1..number).rev() {
        if let Some(commitments) = block_commitments(provider, candidate)? {
            let hash = provider
                .sealed_header(candidate)?
                .map(|header| header.hash())
                .ok_or_eyre(format!("missing prior batch-boundary header {candidate}"))?;
            return Ok((
                candidate,
                hash,
                commitments.processed_deposit_hash,
                commitments.processed_deposit_number,
                commitments.processed_token_count,
                commitments
                    .withdrawal
                    .expect("boundary commitments include withdrawal finalization")
                    .1,
            ));
        }
    }
    // A fresh ZonePortal has not accepted any zone tip yet, so its blockHash is zero. The first
    // batch must extend that on-chain value rather than the local zone genesis hash.
    Ok((0, B256::ZERO, B256::ZERO, 0, 0, 0))
}

/// Certify a zone batch transition independently of whether its predecessor has landed on L1.
/// Live signer/verifier configuration and L1 anchors still apply; portal position is enforced
/// when submitting the batch, not when collecting signatures for it.
pub(crate) async fn build_settlement_attestation<P, V>(
    provider: &P,
    number: u64,
    context: &AttestationContext,
    anchor: (u64, B256),
    proposed_verifier_config: V,
) -> eyre::Result<Option<SettlementAttestation>>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider + StateProviderFactory,
    V: Into<ProposedVerifierConfig>,
{
    let ProposedVerifierConfig::Hash(proposed_verifier_config_hash) =
        proposed_verifier_config.into();
    let Some(commitments) = block_commitments(provider, number)? else {
        return Ok(None);
    };
    let (withdrawal_queue_hash, withdrawal_batch_index) = commitments
        .withdrawal
        .expect("boundary commitments include withdrawal finalization");
    let next_tip = provider
        .sealed_header(number)?
        .ok_or_eyre(format!("missing batch-tip header {number}"))?
        .hash();
    let boundary_state = provider.state_by_block_hash(next_tip)?;
    let imported_tempo = boundary_state
        .tempo_num_hash()
        .map_err(|error| eyre::eyre!(error))?;
    let state_processed_deposit_hash = boundary_state
        .storage(
            ZONE_INBOX_ADDRESS,
            zone_precompiles::inbox::slots::PROCESSED_DEPOSIT_QUEUE_HASH.into(),
        )?
        .map(B256::from)
        .unwrap_or_default();
    let state_processed_deposit_number = boundary_state
        .storage(
            ZONE_INBOX_ADDRESS,
            zone_precompiles::inbox::slots::PROCESSED_DEPOSIT_NUMBER.into(),
        )?
        .unwrap_or_default()
        .to::<u64>();
    let state_processed_token_count = boundary_state
        .storage(
            ZONE_INBOX_ADDRESS,
            zone_precompiles::inbox::slots::PROCESSED_ENABLED_TOKEN_COUNT.into(),
        )?
        .unwrap_or_default()
        .to::<u64>();
    eyre::ensure!(
        imported_tempo.number == commitments.tempo_block_number
            && imported_tempo.hash == commitments.tempo_block_hash
            && state_processed_deposit_hash == commitments.processed_deposit_hash
            && state_processed_deposit_number == commitments.processed_deposit_number
            && state_processed_token_count == commitments.processed_token_count,
        "boundary event commitments do not match canonical Zone state at batch block {number}: event NumHash=({}, {}), state NumHash=({}, {})",
        commitments.tempo_block_number,
        commitments.tempo_block_hash,
        imported_tempo.number,
        imported_tempo.hash
    );
    let (
        previous_height,
        previous_tip,
        previous_deposit_hash,
        previous_deposit_number,
        previous_token_count,
        previous_batch_index,
    ) = previous_batch(provider, number)?;
    let expected_batch_index = previous_batch_index
        .checked_add(1)
        .ok_or_eyre("previous zone withdrawal batch index overflow")?;
    eyre::ensure!(
        withdrawal_batch_index == expected_batch_index,
        "zone withdrawal batch index {withdrawal_batch_index} does not follow previous zone batch index {previous_batch_index}"
    );
    let portal = ZonePortal::new(context.domain.portal_address, context.l1_provider.clone());
    let finalized = context
        .l1_provider
        .get_header_by_number(BlockNumberOrTag::Finalized)
        .await?
        .ok_or_eyre("finalized L1 header is unavailable")?;
    eyre::ensure!(
        finalized.number() >= commitments.tempo_block_number,
        "zone batch imported L1 block {}, but finalized head is {}",
        commitments.tempo_block_number,
        finalized.number()
    );
    let imported = context
        .l1_provider
        .get_header_by_number(commitments.tempo_block_number.into())
        .await?
        .ok_or_eyre(format!(
            "imported L1 header {} is unavailable",
            commitments.tempo_block_number
        ))?;
    eyre::ensure!(
        imported.hash_slow() == commitments.tempo_block_hash,
        "zone batch's imported L1 hash is not canonical"
    );
    let block = BlockId::hash_canonical(commitments.tempo_block_hash);
    let live_header = context
        .l1_provider
        .get_header_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_eyre("latest L1 header is unavailable")?;
    let live_block = BlockId::hash_canonical(live_header.hash_slow());
    let settlement_abi =
        SettlementAbi::from_hardfork(context.chain_spec.tempo_hardfork_at(imported.timestamp()));
    let verifier = portal.verifier().block(block).call().await?;
    let fast_epoch = portal.fastEpoch().block(block).call().await?;
    let fast_active = portal.fastEpochActive().block(block).call().await?;
    eyre::ensure!(
        portal.fastEpoch().block(live_block).call().await? == fast_epoch
            && portal.fastEpochActive().block(live_block).call().await? == fast_active,
        "live fast authority does not match the batch's imported Tempo configuration"
    );
    let (
        set_version,
        fast_epoch,
        roster_hash,
        previous_zone_height,
        previous_block_hash,
        previous_withdrawal_batch_index,
    ) = if fast_active {
        eyre::ensure!(fast_epoch != 0, "active fast authority has epoch zero");
        let config = zone_sequencer::attestation::read_historical_fast_epoch_config(
            &context.l1_provider,
            context.domain.portal_address,
            fast_epoch,
            block,
        )
        .await?;
        let live_config = zone_sequencer::attestation::read_historical_fast_epoch_config(
            &context.l1_provider,
            context.domain.portal_address,
            fast_epoch,
            live_block,
        )
        .await?;
        eyre::ensure!(
            live_config.finalSettlementHash.is_zero(),
            "fast epoch recorded its final settlement; further settlement signing is fenced"
        );
        let proof_policy =
            zone_sequencer::attestation::FastSettlementProofPolicy::from_config(&config)?;
        let verifier_mode = proof_policy.mode.verifier_mode();
        proof_policy.validate_mode(verifier_mode)?;
        eyre::ensure!(
            proposed_verifier_config_hash == proof_policy.expected_verifier_config_hash,
            "settlement proposal verifier config hash does not match historical epoch enrollment"
        );
        let verifier_code = context
            .l1_provider
            .get_code_at(verifier)
            .block_id(block)
            .await?;
        let live_verifier_code = context
            .l1_provider
            .get_code_at(verifier)
            .block_id(live_block)
            .await?;
        eyre::ensure!(
            alloy_primitives::keccak256(&verifier_code) == proof_policy.expected_verifier_code_hash
                && alloy_primitives::keccak256(&live_verifier_code)
                    == proof_policy.expected_verifier_code_hash,
            "historical or live settlement verifier code does not match epoch enrollment"
        );
        eyre::ensure!(
            portal.verifier().block(live_block).call().await? == verifier,
            "live verifier changed from the batch's imported fast enrollment"
        );
        eyre::ensure!(
            config.threshold == 2,
            "fast epoch threshold is {}, expected exactly 2",
            config.threshold
        );
        eyre::ensure!(
            config.finalSettlementHash.is_zero(),
            "fast epoch recorded its final settlement; further settlement is fenced"
        );
        let count = portal
            .fastEpochMemberCount(fast_epoch)
            .block(block)
            .call()
            .await?;
        eyre::ensure!(
            count == U256::from(3),
            "fast epoch must contain exactly 3 members"
        );
        let mut members = Vec::with_capacity(3);
        for index in 0..3 {
            let member = portal
                .fastEpochMemberAt(fast_epoch, U256::from(index))
                .block(block)
                .call()
                .await?;
            eyre::ensure!(
                !member.is_zero() && !members.contains(&member),
                "fast epoch roster contains a zero or duplicate member"
            );
            members.push(member);
        }
        if let Some(signer) = &context.signer {
            eyre::ensure!(
                members.contains(&signer.address()),
                "local settlement signer is not a member of the imported fast roster"
            );
        }
        let guard = context
            .committed_guard
            .as_ref()
            .ok_or_eyre("T14 settlement signer has no runtime committed-prefix guard")?;
        guard(number, next_tip)?;

        let accepted_height = portal.zoneHeight().block(live_block).call().await?;
        let accepted_hash = portal.blockHash().block(live_block).call().await?;
        let accepted_withdrawal_index = portal
            .withdrawalBatchIndex()
            .block(live_block)
            .call()
            .await?;
        eyre::ensure!(
            accepted_height == U256::from(previous_height)
                && accepted_hash == previous_tip
                && accepted_withdrawal_index == previous_batch_index,
            "fast settlement does not extend the exact Portal prefix at the imported L1 anchor"
        );
        (
            0,
            fast_epoch,
            config.rosterHash,
            accepted_height,
            accepted_hash,
            accepted_withdrawal_index,
        )
    } else {
        VerifierMode::try_from(proposed_verifier_config_hash)?;
        let set_version = portal.sequencerSetVersion().block(block).call().await?;
        validate_sequencer_set_version(context.pinned_sequencer_set_version, set_version)?;
        (set_version, 0, B256::ZERO, U256::ZERO, B256::ZERO, 0)
    };

    let (anchor_block_number, anchor_block_hash) = anchor;
    validate_settlement_anchor(
        context,
        commitments.tempo_block_number,
        commitments.tempo_block_hash,
        anchor_block_number,
        anchor_block_hash,
    )
    .await?;

    Ok(Some(SettlementAttestation {
        zoneId: context.domain.zone_id,
        sequencerSetVersion: set_version,
        fastEpoch: fast_epoch,
        rosterHash: roster_hash,
        previousZoneHeight: previous_zone_height,
        previousBlockHash: previous_block_hash,
        previousWithdrawalBatchIndex: previous_withdrawal_batch_index,
        zoneHeight: U256::from(number),
        withdrawalBatchIndex: U256::from(withdrawal_batch_index),
        verifier,
        tempoBlockNumber: commitments.tempo_block_number,
        anchorBlockNumber: anchor_block_number,
        anchorBlockHash: anchor_block_hash,
        blockTransitionHash: alloy_primitives::keccak256((previous_tip, next_tip).abi_encode()),
        depositQueueTransitionHash: alloy_primitives::keccak256(
            (
                previous_deposit_hash,
                commitments.processed_deposit_hash,
                previous_deposit_number,
                commitments.processed_deposit_number,
            )
                .abi_encode(),
        ),
        tokenEnablementTransitionHash: settlement_abi
            .token_transition_hash(previous_token_count, commitments.processed_token_count),
        withdrawalQueueHash: withdrawal_queue_hash,
        verifierConfigHash: proposed_verifier_config_hash,
    }))
}

fn validate_sequencer_set_version(
    pinned_version: Option<u64>,
    live_version: u64,
) -> eyre::Result<()> {
    if let Some(pinned_version) = pinned_version {
        eyre::ensure!(
            live_version == pinned_version,
            "portal signer-set version {live_version} does not match startup-pinned version {pinned_version}"
        );
    }
    Ok(())
}

/// Verify that the proposed Tempo and anchor endpoints are canonical and that the anchor remains
/// available through EIP-2935. The prover verifies the full ancestry between those endpoints.
async fn validate_settlement_anchor(
    context: &AttestationContext,
    tempo_block_number: u64,
    tempo_block_hash: B256,
    anchor_block_number: u64,
    anchor_block_hash: B256,
) -> eyre::Result<()> {
    let current_l1_block = context.l1_provider.get_block_number().await?;
    validate_settlement_anchor_height(
        tempo_block_number,
        anchor_block_number,
        current_l1_block,
        context.anchor_config.history_window(),
    )?;

    let anchor_header = context
        .l1_provider
        .get_header_by_number(anchor_block_number.into())
        .await?
        .ok_or_eyre(format!(
            "missing proposed L1 anchor header {anchor_block_number}"
        ))?
        .inner
        .inner;
    eyre::ensure!(
        anchor_header.hash_slow() == anchor_block_hash,
        "proposed L1 anchor hash does not match finalized L1"
    );

    let tempo_header = context
        .l1_provider
        .get_header_by_number(tempo_block_number.into())
        .await?
        .ok_or_eyre(format!("missing Tempo header {tempo_block_number}"))?
        .inner
        .inner;
    eyre::ensure!(
        tempo_header.hash_slow() == tempo_block_hash,
        "zone batch's Tempo block hash does not match finalized L1"
    );
    Ok(())
}

fn validate_settlement_anchor_height(
    tempo_block_number: u64,
    anchor_block_number: u64,
    current_l1_block: u64,
    history_window: u64,
) -> eyre::Result<()> {
    eyre::ensure!(
        anchor_block_number >= tempo_block_number,
        "proposed L1 anchor predates the zone batch's Tempo block"
    );
    eyre::ensure!(
        anchor_block_number <= current_l1_block,
        "proposed L1 anchor is ahead of the current L1 tip"
    );
    eyre::ensure!(
        current_l1_block.saturating_sub(anchor_block_number) < history_window,
        "proposed L1 anchor fell outside the EIP-2935 history window"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, Log};
    use alloy_provider::{ProviderBuilder, mock::Asserter};
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use tempo_alloy::TempoNetwork;
    use tempo_primitives::{TempoPrimitives, TempoReceipt, TempoTxType};
    use zone_p2p::ZoneManifest;

    fn set_boundary_state(
        provider: &MockEthProvider<TempoPrimitives>,
        tempo_hash: B256,
        tempo_number: u64,
        deposit_hash: B256,
        deposit_number: u64,
        token_count: u64,
    ) {
        provider.add_account(
            tempo_zone_contracts::TEMPO_STATE_ADDRESS,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([
                (
                    zone_precompiles::tempo_state::slots::TEMPO_BLOCK_NUMBER.into(),
                    U256::from(tempo_number),
                ),
                (
                    zone_precompiles::tempo_state::slots::TEMPO_BLOCK_HASH.into(),
                    tempo_hash.into(),
                ),
            ]),
        );
        provider.add_account(
            ZONE_INBOX_ADDRESS,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([
                (
                    zone_precompiles::inbox::slots::PROCESSED_DEPOSIT_QUEUE_HASH.into(),
                    deposit_hash.into(),
                ),
                (
                    zone_precompiles::inbox::slots::PROCESSED_DEPOSIT_NUMBER.into(),
                    U256::from(deposit_number),
                ),
                (
                    zone_precompiles::inbox::slots::PROCESSED_ENABLED_TOKEN_COUNT.into(),
                    U256::from(token_count),
                ),
            ]),
        );
    }

    #[test]
    fn settlement_anchor_accepts_current_tip_and_rejects_future() {
        validate_settlement_anchor_height(100, 100, 100, 10).unwrap();

        let err = validate_settlement_anchor_height(100, 101, 100, 10).unwrap_err();
        assert!(
            err.to_string()
                .contains("proposed L1 anchor is ahead of the current L1 tip")
        );
    }

    #[test]
    fn checkpoint_only_block_is_not_a_settlement_boundary() {
        let provider = MockEthProvider::<TempoPrimitives>::new();
        provider.add_receipts(1, Vec::new());
        assert!(block_commitments(&provider, 1).unwrap().is_none());
    }

    #[test]
    fn same_anchor_batch_boundary_uses_exact_canonical_zone_state() {
        let provider = MockEthProvider::<TempoPrimitives>::new();
        let mut header = TempoHeader::default();
        header.inner.number = 1;
        provider.add_header(header.hash_slow(), header);
        provider.add_receipts(
            1,
            vec![TempoReceipt {
                tx_type: TempoTxType::Legacy,
                success: true,
                cumulative_gas_used: 0,
                logs: vec![Log {
                    address: ZONE_OUTBOX_ADDRESS,
                    data: IZoneOutbox::BatchFinalized {
                        withdrawalQueueHash: B256::repeat_byte(0x31),
                        withdrawalBatchIndex: 1,
                    }
                    .encode_log_data(),
                }],
            }],
        );
        set_boundary_state(
            &provider,
            B256::repeat_byte(0x21),
            101,
            B256::repeat_byte(0x11),
            7,
            3,
        );

        let commitments = block_commitments(&provider, 1).unwrap().unwrap();
        assert_eq!(commitments.tempo_block_number, 101);
        assert_eq!(commitments.tempo_block_hash, B256::repeat_byte(0x21));
        assert_eq!(commitments.processed_deposit_hash, B256::repeat_byte(0x11));
        assert_eq!(commitments.processed_deposit_number, 7);
        assert_eq!(commitments.processed_token_count, 3);
    }

    #[test]
    fn legacy_tempo_advanced_preserves_zero_token_cursor() {
        let provider = MockEthProvider::<TempoPrimitives>::new();
        let tempo_advanced = LegacyTempoAdvanced {
            tempoBlockHash: B256::repeat_byte(0x21),
            tempoBlockNumber: 101,
            depositsProcessed: U256::ZERO,
            newProcessedDepositQueueHash: B256::ZERO,
            lastProcessedDepositNumber: 0,
        };
        let batch_finalized = IZoneOutbox::BatchFinalized {
            withdrawalQueueHash: B256::ZERO,
            withdrawalBatchIndex: 1,
        };
        provider.add_receipts(
            1,
            vec![TempoReceipt {
                tx_type: TempoTxType::Legacy,
                success: true,
                cumulative_gas_used: 0,
                logs: vec![
                    Log {
                        address: ZONE_INBOX_ADDRESS,
                        data: tempo_advanced.encode_log_data(),
                    },
                    Log {
                        address: ZONE_OUTBOX_ADDRESS,
                        data: batch_finalized.encode_log_data(),
                    },
                ],
            }],
        );

        let commitments = block_commitments(&provider, 1).unwrap().unwrap();
        assert_eq!(commitments.processed_token_count, 0);
    }

    #[test]
    fn previous_batch_skips_checkpoint_only_blocks_across_t13() {
        let provider = MockEthProvider::<TempoPrimitives>::new();

        let mut first_boundary_header = TempoHeader::default();
        first_boundary_header.inner.number = 1;
        let first_boundary_hash = first_boundary_header.hash_slow();
        provider.add_header(first_boundary_hash, first_boundary_header);

        let first_deposit_hash = B256::repeat_byte(0x11);
        let first_tempo_advanced = LegacyTempoAdvanced {
            tempoBlockHash: B256::repeat_byte(0x21),
            tempoBlockNumber: 101,
            depositsProcessed: U256::from(3),
            newProcessedDepositQueueHash: first_deposit_hash,
            lastProcessedDepositNumber: 7,
        };
        let first_batch_finalized = IZoneOutbox::BatchFinalized {
            withdrawalQueueHash: B256::repeat_byte(0x31),
            withdrawalBatchIndex: 1,
        };
        provider.add_receipts(
            1,
            vec![TempoReceipt {
                tx_type: TempoTxType::Legacy,
                success: true,
                cumulative_gas_used: 0,
                logs: vec![
                    Log {
                        address: ZONE_INBOX_ADDRESS,
                        data: first_tempo_advanced.encode_log_data(),
                    },
                    Log {
                        address: ZONE_OUTBOX_ADDRESS,
                        data: first_batch_finalized.encode_log_data(),
                    },
                ],
            }],
        );

        for number in [2, 3] {
            let mut header = TempoHeader::default();
            header.inner.number = number;
            provider.add_header(header.hash_slow(), header);
            provider.add_receipts(number, Vec::new());
        }

        let mut current_boundary_header = TempoHeader::default();
        current_boundary_header.inner.number = 4;
        provider.add_header(current_boundary_header.hash_slow(), current_boundary_header);
        let current_tempo_advanced = TempoAdvanced {
            tempoBlockHash: B256::repeat_byte(0x24),
            tempoBlockNumber: 104,
            depositsProcessed: U256::from(5),
            newProcessedDepositQueueHash: B256::repeat_byte(0x14),
            lastProcessedDepositNumber: 12,
            lastProcessedEnabledTokenCount: 15,
        };
        let current_batch_finalized = IZoneOutbox::BatchFinalized {
            withdrawalQueueHash: B256::repeat_byte(0x34),
            withdrawalBatchIndex: 2,
        };
        provider.add_receipts(
            4,
            vec![TempoReceipt {
                tx_type: TempoTxType::Legacy,
                success: true,
                cumulative_gas_used: 0,
                logs: vec![
                    Log {
                        address: ZONE_INBOX_ADDRESS,
                        data: current_tempo_advanced.encode_log_data(),
                    },
                    Log {
                        address: ZONE_OUTBOX_ADDRESS,
                        data: current_batch_finalized.encode_log_data(),
                    },
                ],
            }],
        );

        let commitments = block_commitments(&provider, 4).unwrap().unwrap();
        assert_eq!(commitments.processed_token_count, 15);
        let previous = previous_batch(&provider, 4).unwrap();
        assert_eq!(
            previous,
            (1, first_boundary_hash, first_deposit_hash, 7, 0, 1)
        );
        assert_eq!(
            SettlementAbi::T13.token_transition_hash(previous.4, commitments.processed_token_count),
            alloy_primitives::keccak256((0_u64, 15_u64).abi_encode())
        );
    }

    fn attestation_fixture(
        indices: [u64; 2],
    ) -> (
        MockEthProvider<TempoPrimitives>,
        AttestationContext,
        Asserter,
        TempoHeader,
    ) {
        let provider = MockEthProvider::<TempoPrimitives>::new();
        let mut l1_header = TempoHeader::default();
        l1_header.inner.number = 104;
        for (number, index) in (1_u64..).zip(indices) {
            let mut header = TempoHeader::default();
            header.inner.number = number;
            provider.add_header(header.hash_slow(), header);
            provider.add_receipts(
                number,
                vec![TempoReceipt {
                    tx_type: TempoTxType::Legacy,
                    success: true,
                    cumulative_gas_used: 0,
                    logs: vec![
                        Log {
                            address: ZONE_INBOX_ADDRESS,
                            data: TempoAdvanced {
                                tempoBlockHash: l1_header.hash_slow(),
                                tempoBlockNumber: 104,
                                depositsProcessed: U256::from(10),
                                newProcessedDepositQueueHash: B256::repeat_byte(number as u8),
                                lastProcessedDepositNumber: number * 10,
                                lastProcessedEnabledTokenCount: number * 3,
                            }
                            .encode_log_data(),
                        },
                        Log {
                            address: ZONE_OUTBOX_ADDRESS,
                            data: IZoneOutbox::BatchFinalized {
                                withdrawalQueueHash: B256::repeat_byte((number + 10) as u8),
                                withdrawalBatchIndex: index,
                            }
                            .encode_log_data(),
                        },
                    ],
                }],
            );
        }
        let l1 = Asserter::new();
        let context = AttestationContext::new(
            AttestationDomain {
                l1_chain_id: 42431,
                portal_address: alloy_primitives::Address::repeat_byte(7),
                zone_id: 7,
            },
            Some(1),
            None,
            HashMap::new(),
            ProviderBuilder::new_with_network::<TempoNetwork>()
                .connect_mocked_client(l1.clone())
                .erased(),
            Arc::new(ZoneChainSpec {
                inner: tempo_chainspec::spec::DEV.clone(),
            }),
            BatchAnchorConfig::default(),
        );
        (provider, context, l1, l1_header)
    }

    #[tokio::test]
    async fn settlement_builds_successive_batches_without_portal_progress() {
        let (provider, context, l1, l1_header) = attestation_fixture([1, 2]);
        let verifier = alloy_primitives::Address::repeat_byte(8);
        let mut previous_tip = B256::ZERO;
        let mut previous_deposit = B256::ZERO;
        for number in 1_u64..=2 {
            set_boundary_state(
                &provider,
                l1_header.hash_slow(),
                104,
                B256::repeat_byte(number as u8),
                number * 10,
                number * 3,
            );
            let header = tempo_alloy::rpc::TempoHeaderResponse {
                inner: alloy_rpc_types_eth::Header::new(l1_header.clone()),
                timestamp_millis: 0,
            };
            // Finalized and hash-canonical imported-anchor reads.
            l1.push_success(&header);
            l1.push_success(&header);
            l1.push_success(&header);
            l1.push_success(&Bytes::from(verifier.abi_encode()));
            l1.push_success(&Bytes::from(0_u64.abi_encode()));
            l1.push_success(&Bytes::from(false.abi_encode()));
            l1.push_success(&Bytes::from(0_u64.abi_encode()));
            l1.push_success(&Bytes::from(false.abi_encode()));
            l1.push_success(&Bytes::from(1_u64.abi_encode()));
            // Canonical settlement-anchor validation.
            l1.push_success(&104_u64);
            l1.push_success(&header);
            l1.push_success(&header);

            let attestation = build_settlement_attestation(
                &provider,
                number,
                &context,
                (104, l1_header.hash_slow()),
                VerifierMode::NitroV1,
            )
            .await
            .unwrap()
            .unwrap();
            let next_tip = provider.sealed_header(number).unwrap().unwrap().hash();
            let next_deposit = B256::repeat_byte(number as u8);
            assert_eq!(attestation.zoneHeight, U256::from(number));
            assert_eq!(attestation.withdrawalBatchIndex, U256::from(number));
            assert_eq!(attestation.sequencerSetVersion, 1);
            assert_eq!(attestation.verifier, verifier);
            assert_eq!(
                attestation.blockTransitionHash,
                alloy_primitives::keccak256((previous_tip, next_tip).abi_encode())
            );
            assert_eq!(
                attestation.depositQueueTransitionHash,
                alloy_primitives::keccak256(
                    (
                        previous_deposit,
                        next_deposit,
                        (number - 1) * 10,
                        number * 10
                    )
                        .abi_encode()
                )
            );
            assert_eq!(
                attestation.tokenEnablementTransitionHash,
                SettlementAbi::T13.token_transition_hash((number - 1) * 3, number * 3)
            );
            assert!(l1.read_q().is_empty());
            previous_tip = next_tip;
            previous_deposit = next_deposit;
        }
    }

    #[tokio::test]
    async fn settlement_rejects_nonconsecutive_zone_batch_indices() {
        for (indices, number, expected_error) in [
            ([2, 3], 1, "does not follow previous zone batch index 0"),
            ([1, 3], 2, "does not follow previous zone batch index 1"),
            ([1, 1], 2, "does not follow previous zone batch index 1"),
            (
                [u64::MAX, 0],
                2,
                "previous zone withdrawal batch index overflow",
            ),
        ] {
            let (provider, context, _, l1_header) = attestation_fixture(indices);
            set_boundary_state(
                &provider,
                l1_header.hash_slow(),
                104,
                B256::repeat_byte(number as u8),
                number * 10,
                number * 3,
            );
            let error = build_settlement_attestation(
                &provider,
                number,
                &context,
                (104, l1_header.hash_slow()),
                VerifierMode::NitroV1,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains(expected_error), "{error}");
        }
    }

    fn test_manifest() -> ZoneManifest {
        let public_keys = [1_u64, 2, 3].map(|seed| PrivateKey::from_seed(seed).public_key());
        let mut input = format!(
            "zone_id = 7\nsequencer_set_version = 0\nleader_ed25519_public_key = \"{}\"\n",
            const_hex::encode_prefixed(public_keys[0].as_ref())
        );
        for (index, public_key) in public_keys.iter().enumerate() {
            let number = index + 1;
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{number}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"0x{number:040x}\"\naddress = \"127.0.0.1:{}\"\n",
                const_hex::encode_prefixed(public_key.as_ref()),
                9200 + index,
            ));
        }
        ZoneManifest::parse(&input).expect("valid test manifest")
    }

    #[tokio::test]
    async fn undeployed_portal_is_rejected_before_p2p_startup() {
        let asserter = Asserter::new();
        asserter.push_success(&Bytes::new());
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let portal_address = alloy_primitives::Address::repeat_byte(0x11);

        let err = validate_registered_sequencer_set(&test_manifest(), portal_address, &provider)
            .await
            .expect_err("an undeployed configured portal must prevent P2P startup");

        assert!(
            err.to_string().contains(&format!(
                "ZonePortal {portal_address} is not deployed at the current L1 tip"
            )),
            "unexpected error: {err}"
        );
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn synthetic_test_harness_can_skip_an_unconfigured_portal() {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();

        let pinned_version = validate_registered_sequencer_set(
            &test_manifest(),
            alloy_primitives::Address::ZERO,
            &provider,
        )
        .await
        .expect("synthetic nodes have no configured ZonePortal");
        assert_eq!(pinned_version, None);

        assert!(
            asserter.read_q().is_empty(),
            "the zero-address test bypass must not issue an L1 request"
        );
    }

    #[test]
    fn settlement_rejects_runtime_sequencer_set_rotation() {
        validate_sequencer_set_version(Some(7), 7).expect("unchanged version must remain valid");
        let err = validate_sequencer_set_version(Some(7), 8)
            .expect_err("runtime rotation must fail closed");
        assert!(err.to_string().contains("startup-pinned version 7"));
        validate_sequencer_set_version(None, 8)
            .expect("synthetic nodes without a Portal have no pinned version");
    }
}
