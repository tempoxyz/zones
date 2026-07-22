//! Batch-boundary settlement attestation construction and leader-side proposal recovery.

use std::time::Duration;

use alloy_consensus::TxReceipt as _;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{B256, Bytes, Sealable as _, U256};
use alloy_provider::Provider as _;
use alloy_sol_types::{SolEvent as _, SolValue as _};
use futures::StreamExt as _;
use reth_chain_state::PersistedBlockSubscriptions;
use reth_provider::HeaderProvider;
use reth_storage_api::{BlockNumReader, ReceiptProvider};
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::{
    ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZoneInbox, ZoneOutbox, ZonePortal,
};
use tokio::sync::mpsc;
use tracing::{debug, info};
use zone_p2p::P2pCommand;

use crate::replication::AttestationContext;
use zone_sequencer::attestation::{SettlementAttestation, SignedSettlementAttestation};

#[derive(Debug, Clone, Copy)]
struct BlockCommitments {
    tempo_block_hash: B256,
    tempo_block_number: u64,
    processed_deposit_hash: B256,
    processed_deposit_number: u64,
    withdrawal: Option<(B256, u64)>,
}

/// Extract commitments produced by the deterministic system transactions in a zone block.
fn block_commitments<P>(provider: &P, number: u64) -> eyre::Result<BlockCommitments>
where
    P: ReceiptProvider,
{
    let receipts = provider
        .receipts_by_block(BlockHashOrNumber::Number(number))?
        .ok_or_else(|| eyre::eyre!("receipts for canonical block {number} are not persisted"))?;
    let mut anchor_hash = None;
    let mut tempo_block_number = None;
    let mut processed_deposit_hash = None;
    let mut processed_deposit_number = None;
    let mut withdrawal = None;

    for receipt in receipts {
        for log in receipt.logs() {
            if log.address == ZONE_INBOX_ADDRESS
                && log.topics().first() == Some(&ZoneInbox::TempoAdvanced::SIGNATURE_HASH)
            {
                let event = ZoneInbox::TempoAdvanced::decode_log(log).map_err(|err| {
                    eyre::eyre!("invalid TempoAdvanced log in block {number}: {err}")
                })?;
                anchor_hash = Some(event.tempoBlockHash);
                tempo_block_number = Some(event.tempoBlockNumber);
                processed_deposit_hash = Some(event.newProcessedDepositQueueHash);
                processed_deposit_number = Some(event.lastProcessedDepositNumber);
            } else if log.address == ZONE_OUTBOX_ADDRESS
                && log.topics().first() == Some(&ZoneOutbox::BatchFinalized::SIGNATURE_HASH)
            {
                let event = ZoneOutbox::BatchFinalized::decode_log(log).map_err(|err| {
                    eyre::eyre!("invalid BatchFinalized log in block {number}: {err}")
                })?;
                withdrawal = Some((event.withdrawalQueueHash, event.withdrawalBatchIndex));
            }
        }
    }

    Ok(BlockCommitments {
        tempo_block_hash: anchor_hash
            .ok_or_else(|| eyre::eyre!("block {number} is missing TempoAdvanced"))?,
        tempo_block_number: tempo_block_number
            .ok_or_else(|| eyre::eyre!("block {number} is missing its Tempo block number"))?,
        processed_deposit_hash: processed_deposit_hash
            .ok_or_else(|| eyre::eyre!("block {number} is missing its deposit commitment"))?,
        processed_deposit_number: processed_deposit_number
            .ok_or_else(|| eyre::eyre!("block {number} is missing its deposit number"))?,
        withdrawal,
    })
}

/// Get the previous batch's (i.e the last block in the previous batch) block_hash,
/// deposit_hash and processed deposit number. These values
/// are used to identify the previous batch while submitting the current batch.
fn previous_batch<P>(provider: &P, number: u64) -> eyre::Result<(B256, B256, u64)>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider,
{
    for candidate in (1..number).rev() {
        let commitments = block_commitments(provider, candidate)?;
        if commitments.withdrawal.is_some() {
            let hash = provider
                .sealed_header(candidate)?
                .map(|header| header.hash())
                .ok_or_else(|| eyre::eyre!("missing prior batch-boundary header {candidate}"))?;
            return Ok((
                hash,
                commitments.processed_deposit_hash,
                commitments.processed_deposit_number,
            ));
        }
    }
    // A fresh ZonePortal has not accepted any zone tip yet, so its blockHash is zero. The first
    // batch must extend that on-chain value rather than the local zone genesis hash.
    Ok((B256::ZERO, B256::ZERO, 0))
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
    let commitments = block_commitments(provider, number)?;
    let Some((withdrawal_queue_hash, withdrawal_batch_index)) = commitments.withdrawal else {
        return Ok(None);
    };
    let next_tip = provider
        .sealed_header(number)?
        .ok_or_else(|| eyre::eyre!("missing batch-tip header {number}"))?
        .hash();
    let (previous_tip, previous_deposit_hash, previous_deposit_number) =
        previous_batch(provider, number)?;

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
    eyre::ensure!(
        set_version == context.domain.sequencer_set_version,
        "portal signer-set version {set_version} does not match manifest version {}",
        context.domain.sequencer_set_version
    );
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
                .ok_or_else(|| eyre::eyre!("missing L1 anchor header {anchor_number}"))?
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
        withdrawalQueueHash: withdrawal_queue_hash,
        verifierConfigHash: alloy_primitives::keccak256(Bytes::new()),
    }))
}

/// Verify that the proposed anchor is the same finalized Tempo chain observed by this node
/// before signing the attestation.
async fn validate_settlement_anchor(
    context: &AttestationContext,
    tempo_block_number: u64,
    tempo_block_hash: B256,
    anchor_block_number: u64,
    anchor_block_hash: B256,
) -> eyre::Result<()> {
    eyre::ensure!(
        anchor_block_number >= tempo_block_number,
        "proposed L1 anchor predates the zone batch's Tempo block"
    );

    let anchor_header = context
        .l1_provider
        .get_header_by_number(anchor_block_number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("missing proposed L1 anchor header {anchor_block_number}"))?
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
        .ok_or_else(|| eyre::eyre!("missing Tempo header {tempo_block_number}"))?
        .inner
        .inner;
    eyre::ensure!(
        tempo_header.hash_slow() == tempo_block_hash,
        "zone batch's Tempo block hash does not match finalized L1"
    );

    let mut parent_hash = tempo_block_hash;
    for block_number in (tempo_block_number + 1)..=anchor_block_number {
        let header = context
            .l1_provider
            .get_header_by_number(block_number.into())
            .await?
            .ok_or_else(|| eyre::eyre!("missing L1 ancestry header {block_number}"))?
            .inner
            .inner;
        eyre::ensure!(
            header.inner.parent_hash == parent_hash,
            "L1 ancestry is broken at block {block_number}"
        );
        parent_hash = header.hash_slow();
    }
    eyre::ensure!(
        parent_hash == anchor_block_hash,
        "proposed L1 anchor is not descended from the zone batch's Tempo block"
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
    // Reconstruct the next unsubmitted boundary after a restart. Portal-tip validation inside
    // the builder ensures only the first still-current batch is proposed.
    let head = match provider.best_block_number() {
        Ok(head) => head,
        Err(err) => {
            tracing::error!(target: "zone::p2p", %err, "Failed reading head for settlement recovery");
            return;
        }
    };

    let mut pending_boundary = None;
    for number in 1..=head {
        match propose_settlement(&provider, number, &commands, &context).await {
            Ok(true) => {
                pending_boundary = Some(number);
                break;
            }
            Ok(false) => {}
            Err(err) => {
                debug!(target: "zone::p2p", %err, number, "Skipped non-current settlement boundary during recovery")
            }
        }
    }

    let mut persisted = provider.persisted_block_stream();
    let mut retry = tokio::time::interval(Duration::from_secs(5));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            tip = persisted.next() => {
                let Some(tip) = tip else { return };
                match propose_settlement(&provider, tip.number, &commands, &context).await {
                    Ok(true) => pending_boundary = Some(tip.number),
                    Ok(false) => {}
                    Err(err) => tracing::warn!(target: "zone::p2p", %err, height = tip.number, "Failed proposing settlement boundary"),
                }
            }
            _ = retry.tick(), if pending_boundary.is_some() => {
                let number = pending_boundary.expect("guarded by is_some");
                match propose_settlement(&provider, number, &commands, &context).await {
                    Ok(true) => {}
                    Ok(false) => pending_boundary = None,
                    Err(err) => {
                        debug!(target: "zone::p2p", %err, height = number, "Settlement proposal retry is not currently valid");

                        // A successful submitBatch makes the previously pending proposal stale.
                        // Walk the already-persisted boundaries after it so the next batch can be
                        // proposed even when the live tip is now far ahead of the portal tip.
                        let head = match provider.best_block_number() {
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
                    }
                }
            }
        }
    }
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
    let signed =
        SignedSettlementAttestation::sign(attestation.clone(), context.domain, &context.signer)?;
    let signer = signed.recover_signer(context.domain)?;
    let (_, signatures) = context
        .store
        .as_ref()
        .expect("leader must have an attestation store")
        .insert_settlement(context.domain, signer, signed);
    commands
        .send(P2pCommand::BroadcastSettlementProposal(
            attestation.encode(),
        ))
        .await
        .map_err(|_| eyre::eyre!("P2P command channel closed"))?;
    info!(target: "zone::p2p", height = number, %signer, signatures, "Signed and broadcast settlement proposal");
    Ok(true)
}
