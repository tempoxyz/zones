//! Batch-boundary settlement attestation construction and leader-side proposal recovery.

use std::{future::Future, time::Duration};

use alloy_consensus::TxReceipt as _;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{B256, Bytes, Sealable as _, U256};
use alloy_provider::Provider as _;
use alloy_sol_types::{SolEvent as _, SolValue as _};
use eyre::{OptionExt as _, WrapErr as _};
use futures::StreamExt as _;
use reth_chain_state::PersistedBlockSubscriptions;
use reth_provider::HeaderProvider;
use reth_storage_api::{BlockNumReader, ReceiptProvider};
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::{
    IZoneOutbox, LegacyTempoAdvanced, TempoAdvanced, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
    ZonePortal,
};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info};
use zone_p2p::P2pCommand;

use crate::replication::AttestationContext;
use zone_sequencer::{
    SettlementAbi,
    attestation::{SettlementAttestation, SignedSettlementAttestation},
};

/// Fallback cadence for transient L1 validation failures or dropped P2P settlement proposals.
const SETTLEMENT_RETRY_INTERVAL: Duration = Duration::from_millis(500);

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
    let sequencer_set_version = portal
        .sequencerSetVersion()
        .call()
        .await
        .wrap_err("failed reading the ZonePortal sequencer-set version")?;
    let threshold_call = portal.sequencerThreshold();
    let count_call = portal.sequencerCount();
    let registered = futures::future::try_join_all(quorum.iter().map(|(node, address)| {
        let (name, address) = (node.name(), *address);
        let call = portal.isSequencer(address);
        async move { call.call().await.map(|ok| (name, address, ok)) }
    }));
    let (threshold, registered_count, registered) =
        tokio::try_join!(threshold_call.call(), count_call.call(), registered)
            .wrap_err("failed reading the registered sequencer set from ZonePortal")?;
    let validated_version = portal
        .sequencerSetVersion()
        .call()
        .await
        .wrap_err("failed re-reading the ZonePortal sequencer-set version")?;
    eyre::ensure!(
        validated_version == sequencer_set_version,
        "ZonePortal sequencer set changed during startup validation ({sequencer_set_version} -> {validated_version})"
    );

    for (name, address, is_registered) in registered {
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
    P: ReceiptProvider,
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
                            format!("invalid post-T12 TempoAdvanced log in block {number}")
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

    Ok(Some(BlockCommitments {
        tempo_block_hash: anchor_hash
            .ok_or_eyre(format!("block {number} is missing TempoAdvanced"))?,
        tempo_block_number: tempo_block_number
            .ok_or_eyre(format!("block {number} is missing its Tempo block number"))?,
        processed_deposit_hash: processed_deposit_hash
            .ok_or_eyre(format!("block {number} is missing its deposit commitment"))?,
        processed_deposit_number: processed_deposit_number
            .ok_or_eyre(format!("block {number} is missing its deposit number"))?,
        processed_token_count: processed_token_count
            .ok_or_eyre(format!("block {number} is missing its token cursor"))?,
        withdrawal: Some(withdrawal),
    }))
}

/// Get the previous batch's (i.e the last block in the previous batch) block_hash,
/// deposit_hash, processed_deposit_number, and processed_token_count. These values
/// are used to identify the previous batch while submitting the current batch.
fn previous_batch<P>(provider: &P, number: u64) -> eyre::Result<(B256, B256, u64, u64)>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider,
{
    for candidate in (1..number).rev() {
        if let Some(commitments) = block_commitments(provider, candidate)? {
            let hash = provider
                .sealed_header(candidate)?
                .map(|header| header.hash())
                .ok_or_eyre(format!("missing prior batch-boundary header {candidate}"))?;
            return Ok((
                hash,
                commitments.processed_deposit_hash,
                commitments.processed_deposit_number,
                commitments.processed_token_count,
            ));
        }
    }
    // A fresh ZonePortal has not accepted any zone tip yet, so its blockHash is zero. The first
    // batch must extend that on-chain value rather than the local zone genesis hash.
    Ok((B256::ZERO, B256::ZERO, 0, 0))
}

/// Build the settlement attestation at a batch boundary in the exact format ZonePortal expects.
pub(crate) async fn build_settlement_attestation<P>(
    provider: &P,
    number: u64,
    context: &AttestationContext,
    proposed_anchor: Option<(u64, B256)>,
) -> eyre::Result<Option<SettlementAttestation>>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider,
{
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
    let (previous_tip, previous_deposit_hash, previous_deposit_number, previous_token_count) =
        previous_batch(provider, number)?;
    let settlement_abi = SettlementAbi::from_l1(&context.l1_provider).await?;

    let portal = ZonePortal::new(context.domain.portal_address, context.l1_provider.clone());
    let set_version_call = portal.sequencerSetVersion();
    let portal_batch_index_call = portal.withdrawalBatchIndex();
    let verifier_call = portal.verifier();
    let portal_tip_call = portal.blockHash();
    let (set_version, portal_batch_index, verifier, portal_tip) = tokio::try_join!(
        set_version_call.call(),
        portal_batch_index_call.call(),
        verifier_call.call(),
        portal_tip_call.call(),
    )?;
    validate_sequencer_set_version(context.pinned_sequencer_set_version, set_version)?;
    eyre::ensure!(
        portal_tip == previous_tip,
        "proposal does not extend the portal batch tip"
    );
    eyre::ensure!(
        withdrawal_batch_index == portal_batch_index.saturating_add(1),
        "zone withdrawal batch index {withdrawal_batch_index} does not follow portal index {portal_batch_index}"
    );

    let (anchor_block_number, anchor_block_hash) = if let Some(anchor) = proposed_anchor {
        anchor
    } else {
        let l1_tip = context.l1_provider.get_block_number().await?;
        let gap = l1_tip.saturating_sub(commitments.tempo_block_number);
        if gap < context.anchor_config.effective_window() {
            (commitments.tempo_block_number, commitments.tempo_block_hash)
        } else {
            let anchor_number = l1_tip.saturating_sub(context.anchor_config.safety_margin());
            let header = context
                .l1_provider
                .get_header_by_number(anchor_number.into())
                .await?
                .ok_or_eyre(format!("missing L1 anchor header {anchor_number}"))?
                .inner
                .inner;
            (anchor_number, header.hash_slow())
        }
    };
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
        verifierConfigHash: alloy_primitives::keccak256(Bytes::new()),
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

/// Long-running async task that detects persisted batch boundaries and broadcasts settlement
/// proposals to followers. At each boundary, it signs the proposal locally and initiates follower
/// attestation collection; follower responses are received by the P2P sync task and inserted into
/// the shared attestation store.
pub(crate) async fn collect_leader_settlements<P>(
    provider: P,
    commands: mpsc::Sender<P2pCommand>,
    context: AttestationContext,
    portal_confirmed_height: u64,
) where
    P: PersistedBlockSubscriptions
        + BlockNumReader
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    // Subscribe before reading the persisted head, then reconcile through that head. Reth's
    // persisted-block stream is latest-value based, so notifications are wake-ups rather than a
    // lossless sequence of every persisted block.
    let mut persisted = provider.persisted_block_stream();
    let store = &context.store;
    let mut submitted_heights = store.subscribe_submitted_height();
    let head = match provider.last_block_number() {
        Ok(head) => head,
        Err(err) => {
            tracing::error!(target: "zone::p2p", %err, "Failed reading head for settlement recovery");
            return;
        }
    };

    // Start at the block after the portal-confirmed anchor.
    let recovery_start = portal_confirmed_height.saturating_add(1);
    let mut pending_boundary =
        propose_persisted_settlement_range(&provider, &commands, &context, recovery_start, head)
            .await;

    let mut last_scanned = head;
    let mut retry = tokio::time::interval(SETTLEMENT_RETRY_INTERVAL);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            tip = persisted.next() => {
                let Some(tip) = tip else { return };
                if tip.number < last_scanned {
                    tracing::error!(target: "zone::p2p", persisted = tip.number, last_scanned, "Persisted zone head moved backwards");
                    return;
                }

                if pending_boundary.is_none() {
                    pending_boundary = propose_persisted_settlement_range(
                        &provider,
                        &commands,
                        &context,
                        last_scanned.saturating_add(1),
                        tip.number,
                    ).await;
                }
                last_scanned = tip.number;
            }
            submitted = wait_for_submitted_height(
                &mut submitted_heights,
                pending_boundary.unwrap_or(u64::MAX),
            ), if pending_boundary.is_some() => {
                let submitted = match submitted {
                    Ok(submitted) => submitted,
                    Err(err) => {
                        tracing::error!(target: "zone::p2p", %err, "Settlement submission notification channel closed");
                        return;
                    }
                };
                let head = match provider.last_block_number() {
                    Ok(head) => head,
                    Err(err) => {
                        tracing::warn!(target: "zone::p2p", %err, "Failed reading persisted head after settlement confirmation");
                        continue;
                    }
                };
                pending_boundary = propose_persisted_settlement_range(
                    &provider,
                    &commands,
                    &context,
                    submitted.saturating_add(1),
                    head,
                ).await;
                last_scanned = head;
            }
            _ = retry.tick(), if pending_boundary.is_some() => {
                let number = pending_boundary.expect("guarded by is_some");
                match propose_settlement(&provider, number, &commands, &context).await {
                    Ok(true) => {}
                    Ok(false) => {
                        // The retained candidate only failed before we could determine whether it
                        // was a boundary. Once a retry classifies it as an ordinary block, resume
                        // the startup scan after it instead of stranding the rest of the range
                        // behind last_scanned.
                        let head = match provider.last_block_number() {
                            Ok(head) => head,
                            Err(err) => {
                                debug!(target: "zone::p2p", %err, "Failed reading head while resuming settlement recovery");
                                continue;
                            }
                        };
                        pending_boundary = propose_persisted_settlement_range(
                            &provider,
                            &commands,
                            &context,
                            number.saturating_add(1),
                            head,
                        )
                        .await;
                        last_scanned = head;
                    }
                    Err(err) => {
                        debug!(target: "zone::p2p", %err, height = number, "Settlement proposal retry is not currently valid");

                        // A successful submitBatch makes the previously pending proposal stale.
                        // Walk the already-persisted boundaries after it so the next batch can be
                        // proposed even when the live tip is now far ahead of the portal tip.
                        let head = match provider.last_block_number() {
                            Ok(head) => head,
                            Err(err) => {
                                debug!(target: "zone::p2p", %err, "Failed reading head while advancing settlement proposal");
                                continue;
                            }
                        };
                        for candidate in number.saturating_add(1)..=head {
                            match propose_settlement(&provider, candidate, &commands, &context).await {
                                Ok(true) => {
                                    pending_boundary = Some(candidate);
                                    break;
                                }
                                Ok(false) => {}
                                Err(err) => debug!(target: "zone::p2p", %err, height = candidate, "Skipped non-current settlement boundary while advancing"),
                            }
                        }
                        last_scanned = head;
                    }
                }
            }
        }
    }
}

/// Wait until the submitter or portal resync confirms at least `pending_height`.
async fn wait_for_submitted_height(
    submitted_heights: &mut watch::Receiver<u64>,
    pending_height: u64,
) -> Result<u64, watch::error::RecvError> {
    loop {
        let submitted_height = *submitted_heights.borrow_and_update();
        if submitted_height >= pending_height {
            return Ok(submitted_height);
        }
        submitted_heights.changed().await?;
    }
}

/// Propose the first batch boundary in an already-persisted range.
///
/// A failed candidate is retained for timer-based retry so a transient L1 failure cannot strand
/// it when no further persisted-block notification arrives.
async fn propose_persisted_settlement_range<P>(
    provider: &P,
    commands: &mpsc::Sender<P2pCommand>,
    context: &AttestationContext,
    start: u64,
    end: u64,
) -> Option<u64>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider,
{
    scan_settlement_range(start, end, |candidate| {
        propose_settlement(provider, candidate, commands, context)
    })
    .await
}

async fn scan_settlement_range<F, Fut>(start: u64, end: u64, mut propose: F) -> Option<u64>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = eyre::Result<bool>>,
{
    for candidate in start..=end {
        match propose(candidate).await {
            Ok(true) => return Some(candidate),
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(target: "zone::p2p", %err, height = candidate, "Failed proposing settlement boundary");
                return Some(candidate);
            }
        }
    }
    None
}

/// Before we settle on L1 with `submitBatch`, we need to collect follower signatures for this
/// batch. Create a settlement proposal and send it to followers, who will sign and return a
/// SettlementAttestation that will be sent along with the submitBatch for the zoneportal's
/// on-chain quorum.
async fn propose_settlement<P>(
    provider: &P,
    number: u64,
    commands: &mpsc::Sender<P2pCommand>,
    context: &AttestationContext,
) -> eyre::Result<bool>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider,
{
    let Some(attestation) = build_settlement_attestation(provider, number, context, None).await?
    else {
        return Ok(false);
    };
    let signer_key = context
        .signer
        .as_ref()
        .ok_or_eyre("this node holds no individual secp256k1 key, so it cannot settle")?;
    let signed =
        SignedSettlementAttestation::sign(attestation.clone(), context.domain, signer_key)?;
    let signer = signed.recover_signer(context.domain)?;
    let (_, signatures) = context
        .store
        .insert_settlement(context.domain, signer, signed);
    commands
        .send(P2pCommand::BroadcastSettlementProposal(
            attestation.encode(),
        ))
        .await
        .wrap_err("P2P command channel closed")?;
    info!(target: "zone::p2p", height = number, %signer, signatures, "Signed and broadcast settlement proposal");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Log;
    use alloy_provider::{ProviderBuilder, mock::Asserter};
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
    use reth_provider::test_utils::MockEthProvider;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tempo_alloy::TempoNetwork;
    use tempo_primitives::{TempoPrimitives, TempoReceipt, TempoTxType};
    use zone_p2p::ZoneManifest;
    use zone_sequencer::attestation::AttestationStore;

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
    fn previous_batch_skips_checkpoint_only_blocks_across_t12() {
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
        assert_eq!(previous, (first_boundary_hash, first_deposit_hash, 7, 0));
        assert_eq!(
            SettlementAbi::T12.token_transition_hash(previous.3, commitments.processed_token_count),
            alloy_primitives::keccak256((0_u64, 15_u64).abi_encode())
        );
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

    #[tokio::test]
    async fn startup_recovery_retries_first_erroring_boundary() {
        let failed_once = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let propose = |number| {
            let failed_once = failed_once.clone();
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push(number);
                if number % 2 != 0 {
                    return Ok(false);
                }
                if number == 2 && !failed_once.swap(true, Ordering::Relaxed) {
                    eyre::bail!("transient proposal failure");
                }
                Ok(number == 2)
            }
        };

        let pending = scan_settlement_range(1, 4, propose).await;
        assert_eq!(pending, Some(2));
        assert_eq!(*calls.lock().unwrap(), vec![1, 2]);

        let pending = scan_settlement_range(pending.unwrap(), 4, propose).await;
        assert_eq!(pending, Some(2));
        assert_eq!(*calls.lock().unwrap(), vec![1, 2, 2]);
    }

    #[tokio::test]
    async fn startup_recovery_begins_after_portal_confirmed_height() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let propose = |number| {
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push(number);
                Ok(number == 360)
            }
        };

        let portal_confirmed_height = 240u64;
        let pending =
            scan_settlement_range(portal_confirmed_height.saturating_add(1), 360, propose).await;

        assert_eq!(pending, Some(360));
        let calls = calls.lock().unwrap();
        assert_eq!(calls.first(), Some(&241));
        assert_eq!(calls.last(), Some(&360));
        assert_eq!(calls.len(), 120);
    }

    #[tokio::test]
    async fn startup_recovery_resumes_after_erroring_non_boundary() -> eyre::Result<()> {
        let failed_once = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let propose = |number| {
            let failed_once = failed_once.clone();
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push(number);
                if number == 2 && !failed_once.swap(true, Ordering::Relaxed) {
                    eyre::bail!("transient proposal failure");
                }
                Ok(number == 4)
            }
        };

        let pending = scan_settlement_range(1, 4, propose).await;
        assert_eq!(pending, Some(2));
        assert_eq!(*calls.lock().unwrap(), vec![1, 2]);

        let number = pending.unwrap();
        assert!(!propose(number).await?);
        let pending = scan_settlement_range(number.saturating_add(1), 4, propose).await;
        assert_eq!(pending, Some(4));
        assert_eq!(*calls.lock().unwrap(), vec![1, 2, 2, 3, 4]);
        Ok(())
    }

    #[tokio::test]
    async fn submission_confirmation_wakes_pending_boundary_without_retry_tick() {
        let store = AttestationStore::default();
        let mut submitted_heights = store.subscribe_submitted_height();
        let waiting =
            tokio::spawn(
                async move { wait_for_submitted_height(&mut submitted_heights, 2_070).await },
            );

        tokio::task::yield_now().await;
        store.remove_submitted(2_068);
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        store.remove_submitted(2_070);
        let submitted = tokio::time::timeout(Duration::from_millis(100), waiting)
            .await
            .expect("confirmation should wake the collector without its fallback retry")
            .expect("submission wait task should not panic")
            .expect("submission notification channel should remain open");
        assert_eq!(submitted, 2_070);
    }
}
