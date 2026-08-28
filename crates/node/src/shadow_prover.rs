//! Finalized L1 batch observation for RPC-only shadow provers.

use std::{collections::HashSet, str::FromStr as _, time::Duration};

use alloy_consensus::{BlockHeader as _, Sealable as _};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{DynProvider, Provider as _};
use alloy_sol_types::SolCall as _;
use eyre::{OptionExt as _, Result, WrapErr as _, ensure};
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::ZonePortal;
use tracing::{error, info, warn};
use zone_sequencer::{
    BatchAnchorConfig, BatchData, ShadowProofAnchor, ShadowProver, ZoneSequencerProvider,
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Observe finalized `submitBatch` calls and feed their exact settlement inputs to the detached
/// prover. The Portal has already verified the included quorum certificate before emitting the
/// event; decoding the call binds the proof job to that accepted certificate.
pub(crate) async fn run_finalized_batch_observer<P: ZoneSequencerProvider>(
    portal_address: Address,
    anchor_config: BatchAnchorConfig,
    zone_provider: P,
    l1_provider: DynProvider<TempoNetwork>,
    prover: ShadowProver,
) {
    let mut next_block = None;
    let mut observed = HashSet::new();

    loop {
        let result = observe_once(
            portal_address,
            anchor_config,
            &zone_provider,
            &l1_provider,
            &prover,
            &mut next_block,
            &mut observed,
        )
        .await;
        if let Err(err) = result {
            warn!(
                target: "zone::node::shadow_prover",
                error = ?err,
                "Failed to observe finalized batch submissions; retrying"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn observe_once<P: ZoneSequencerProvider>(
    portal_address: Address,
    anchor_config: BatchAnchorConfig,
    zone_provider: &P,
    l1_provider: &DynProvider<TempoNetwork>,
    prover: &ShadowProver,
    next_block: &mut Option<u64>,
    observed: &mut HashSet<(B256, u64)>,
) -> Result<()> {
    let finalized = l1_provider
        .get_header_by_number(BlockNumberOrTag::Finalized)
        .await?
        .ok_or_eyre("L1 finalized block is not available")?
        .number();
    let from = next_block.unwrap_or_else(|| {
        finalized.saturating_sub(anchor_config.history_window().saturating_sub(1))
    });
    if from > finalized {
        return Ok(());
    }

    let portal = ZonePortal::new(portal_address, l1_provider);
    let events = portal
        .BatchSubmitted_filter()
        .from_block(from)
        .to_block(finalized)
        .query()
        .await
        .wrap_err_with(|| {
            format!("query BatchSubmitted logs in finalized range {from}..={finalized}")
        })?;

    for (event, log) in events {
        let tx_hash = log
            .transaction_hash
            .ok_or_eyre("finalized BatchSubmitted log has no transaction hash")?;
        let log_index = log
            .log_index
            .ok_or_eyre("finalized BatchSubmitted log has no log index")?;
        let observation_id = (tx_hash, log_index);
        if observed.contains(&observation_id) {
            continue;
        }

        let call = fetch_submit_batch_calls(l1_provider, portal_address, tx_hash)
            .await?
            .into_iter()
            .find(|call| call_matches_event(call, &event))
            .ok_or_eyre(format!(
                "submitBatch transaction {tx_hash} contains no call matching log {log_index}"
            ))?;
        let (from, to, batch, anchor) =
            submission_target(zone_provider, l1_provider, &call, &event, finalized)
                .await
                .wrap_err_with(|| {
                    format!("validate finalized submitBatch transaction {tx_hash}")
                })?;

        observed.insert(observation_id);
        let zone_provider = zone_provider.clone();
        let prover = prover.clone();
        tokio::spawn(async move {
            if let Err(err) = wait_and_enqueue(zone_provider, prover, from, to, batch, anchor).await
            {
                error!(
                    target: "zone::node::shadow_prover",
                    transaction_hash = %tx_hash,
                    error = ?err,
                    "Could not enqueue finalized batch for shadow proving"
                );
            }
        });
    }

    *next_block = Some(finalized.saturating_add(1));
    Ok(())
}

async fn fetch_submit_batch_calls(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    tx_hash: B256,
) -> Result<Vec<ZonePortal::submitBatchCall>> {
    let tx = provider
        .client()
        .request::<_, serde_json::Value>("eth_getTransactionByHash", (tx_hash,))
        .await?
        .as_object()
        .cloned()
        .ok_or_eyre(format!("submitBatch transaction {tx_hash} was not found"))?;

    let mut candidates = Vec::new();
    candidates.push(serde_json::Value::Object(tx.clone()));
    if let Some(calls) = tx.get("calls").and_then(serde_json::Value::as_array) {
        candidates.extend(calls.iter().cloned());
    }

    let mut calls = Vec::new();
    for candidate in candidates {
        let Some(to) = candidate
            .get("to")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| Address::from_str(value).ok())
        else {
            continue;
        };
        if to != portal {
            continue;
        }
        let Some(input) = candidate
            .get("input")
            .and_then(serde_json::Value::as_str)
            .filter(|input| *input != "0x")
        else {
            continue;
        };
        let Ok(calldata) = Bytes::from_str(input) else {
            continue;
        };
        if let Ok(call) = ZonePortal::submitBatchCall::abi_decode(&calldata) {
            calls.push(call);
        }
    }

    ensure!(
        !calls.is_empty(),
        "transaction {tx_hash} contains no submitBatch call to Portal {portal}"
    );
    Ok(calls)
}

fn call_matches_event(
    call: &ZonePortal::submitBatchCall,
    event: &ZonePortal::BatchSubmitted,
) -> bool {
    event.nextBlockHash == call.blockTransition.nextBlockHash
        && event.nextProcessedDepositQueueHash == call.depositQueueTransition.nextProcessedHash
        && event.lastProcessedDepositNumber == call.depositQueueTransition.nextDepositNumber
        && event.withdrawalQueueHash == call.withdrawalQueueHash
}

async fn submission_target<P: ZoneSequencerProvider>(
    zone_provider: &P,
    l1_provider: &DynProvider<TempoNetwork>,
    call: &ZonePortal::submitBatchCall,
    event: &ZonePortal::BatchSubmitted,
    finalized: u64,
) -> Result<(u64, u64, BatchData, ShadowProofAnchor)> {
    ensure!(
        !call.signatures.is_empty(),
        "accepted submitBatch call has an empty quorum certificate"
    );
    let to = u64::try_from(call.nextZoneHeight).wrap_err("submitted Zone height overflows u64")?;
    ensure!(
        event.nextBlockHash == call.blockTransition.nextBlockHash,
        "BatchSubmitted next block hash does not match calldata"
    );
    ensure!(
        event.nextProcessedDepositQueueHash == call.depositQueueTransition.nextProcessedHash,
        "BatchSubmitted deposit hash does not match calldata"
    );
    ensure!(
        event.lastProcessedDepositNumber == call.depositQueueTransition.nextDepositNumber,
        "BatchSubmitted deposit number does not match calldata"
    );
    ensure!(
        event.withdrawalQueueHash == call.withdrawalQueueHash,
        "BatchSubmitted withdrawal hash does not match calldata"
    );

    let from = if call.blockTransition.prevBlockHash.is_zero() {
        1
    } else {
        zone_provider
            .block_number(call.blockTransition.prevBlockHash)?
            .ok_or_eyre(format!(
                "submitted previous Zone block {} is not available locally",
                call.blockTransition.prevBlockHash
            ))?
            .saturating_add(1)
    };
    ensure!(from <= to, "submitted Zone range {from}..={to} is invalid");

    let anchor_number =
        submitted_anchor_number(call.tempoBlockNumber, call.recentTempoBlockNumber)?;
    ensure!(
        anchor_number <= finalized,
        "submitted anchor {anchor_number} is above finalized L1 height {finalized}"
    );
    let anchor_hash = l1_provider
        .get_header_by_number(anchor_number.into())
        .await?
        .ok_or_eyre(format!(
            "submitted Tempo anchor {anchor_number} is unavailable"
        ))?
        .hash_slow();

    Ok((
        from,
        to,
        BatchData {
            zone_height: to,
            tempo_block_number: call.tempoBlockNumber,
            prev_block_hash: call.blockTransition.prevBlockHash,
            next_block_hash: call.blockTransition.nextBlockHash,
            prev_processed_deposit_hash: call.depositQueueTransition.prevProcessedHash,
            next_processed_deposit_hash: call.depositQueueTransition.nextProcessedHash,
            prev_deposit_number: call.depositQueueTransition.prevDepositNumber,
            next_deposit_number: call.depositQueueTransition.nextDepositNumber,
            withdrawal_queue_hash: call.withdrawalQueueHash,
            withdrawal_batch_index: event.withdrawalBatchIndex,
        },
        ShadowProofAnchor {
            number: anchor_number,
            hash: anchor_hash,
        },
    ))
}

async fn wait_and_enqueue<P: ZoneSequencerProvider>(
    zone_provider: P,
    prover: ShadowProver,
    from: u64,
    to: u64,
    batch: BatchData,
    anchor: ShadowProofAnchor,
) -> Result<()> {
    loop {
        let local_head = zone_provider.last_block_number()?;
        if local_head >= to {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    ensure!(
        zone_provider.block_hash(to)? == Some(batch.next_block_hash),
        "local canonical Zone block {to} does not match finalized submission {}",
        batch.next_block_hash
    );
    if from > 1 {
        ensure!(
            zone_provider.block_hash(from - 1)? == Some(batch.prev_block_hash),
            "local parent of Zone range {from}..={to} does not match finalized submission {}",
            batch.prev_block_hash
        );
    }

    info!(
        target: "zone::node::shadow_prover",
        zone_from = from,
        zone_to = to,
        anchor_number = anchor.number,
        anchor_hash = %anchor.hash,
        "Queueing finalized quorum-certified batch for shadow proving"
    );
    prover.enqueue_with_anchor(from, to, batch, anchor).await
}

fn submitted_anchor_number(tempo_block_number: u64, recent_tempo_block_number: u64) -> Result<u64> {
    if recent_tempo_block_number == 0 {
        return Ok(tempo_block_number);
    }
    ensure!(
        recent_tempo_block_number > tempo_block_number,
        "submitted ancestry anchor does not follow its Tempo checkpoint"
    );
    Ok(recent_tempo_block_number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_submission_anchors_to_checkpoint() {
        assert_eq!(submitted_anchor_number(42, 0).unwrap(), 42);
    }

    #[test]
    fn ancestry_submission_uses_committed_recent_block() {
        assert_eq!(submitted_anchor_number(42, 100).unwrap(), 100);
    }

    #[test]
    fn ancestry_submission_must_follow_checkpoint() {
        assert!(submitted_anchor_number(42, 42).is_err());
        assert!(submitted_anchor_number(42, 41).is_err());
    }
}
