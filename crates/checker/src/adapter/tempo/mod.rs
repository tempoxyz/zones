//! Parses authenticated Tempo transaction envelopes into checker facts.

mod withdrawals;

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{B256, U256};

use crate::{
    failure::Failure,
    kernel::{
        BatchSubmission, Cursor, Effect, ImportedFacts, ImportedOperation, PortalIdentity,
        RefundClaim, TokenEnable, Withdrawal, WithdrawalProcessing,
    },
    observe::{
        L1BlockObservation,
        events::{Factory, L1ProtocolEvent, Portal},
    },
};

use super::{AdapterFindingCode, ImportedAdaptation, deposits::ordinary_deposit_event};
use withdrawals::{WithdrawalAdaptation, parse_withdrawal_events};

/// Parse imported transaction envelopes into ordered kernel facts and effects.
pub(super) fn facts(
    observation: &L1BlockObservation,
    header: &crate::observe::ImportedTempoHeader,
    portal_creation_block_hash: B256,
    zone_id: u32,
) -> Result<ImportedAdaptation, Failure> {
    let mut operations = Vec::new();
    let mut effects = Vec::new();
    for tx in observation.protocol_transactions() {
        let events: Vec<_> = tx.outcomes().iter().map(|x| x.event()).collect();
        let direct_call = tx.direct_call();
        let is_creation_block = observation.block_hash() == portal_creation_block_hash;
        let creation_event_index = events
            .iter()
            .position(|event| matches!(event, L1ProtocolEvent::FactoryZoneCreated(_)));
        let is_creation_transaction = creation_event_index.is_some();
        if let Some(call) = direct_call {
            if let Some(call) = call.as_submit_batch() {
                if !matches!(
                    events.as_slice(),
                    [L1ProtocolEvent::Portal(
                        Portal::ZonePortalEvents::BatchSubmitted(_)
                    )]
                ) {
                    return Err(AdapterFindingCode::Grammar
                        .failure("submitBatch requires exactly one BatchSubmitted event"));
                }
                operations.push(ImportedOperation::SubmitBatch(BatchSubmission {
                    tempo_block: call.tempoBlockNumber,
                    previous_block: call.blockTransition.prevBlockHash,
                    next_block: call.blockTransition.nextBlockHash,
                    previous_deposit: Cursor {
                        hash: call.depositQueueTransition.prevProcessedHash,
                        number: call.depositQueueTransition.prevDepositNumber,
                    },
                    next_deposit: Cursor {
                        hash: call.depositQueueTransition.nextProcessedHash,
                        number: call.depositQueueTransition.nextDepositNumber,
                    },
                    withdrawal_queue_hash: call.withdrawalQueueHash,
                    next_zone_height: call.nextZoneHeight,
                }));
            } else if let Some(call) = call.as_process_withdrawals() {
                let withdrawals = call
                    .withdrawals
                    .iter()
                    .map(|w| Withdrawal {
                        token: w.token,
                        sender_tag: w.senderTag,
                        to: w.to,
                        amount: w.amount,
                        memo: w.memo,
                        gas_limit: w.gasLimit,
                        fallback_nonce: w.fallbackNonce,
                        callback_data: w.callbackData.clone(),
                        encrypted_sender: w.encryptedSender.clone(),
                    })
                    .collect();
                let WithdrawalAdaptation {
                    outcomes,
                    effects: processing_effects,
                } = parse_withdrawal_events(
                    &events,
                    call.withdrawals.len(),
                    observation.portal_address(),
                )?;
                effects.extend(processing_effects);
                operations.push(ImportedOperation::ProcessWithdrawals(
                    WithdrawalProcessing {
                        base_fee: U256::from(header.header().base_fee_per_gas().ok_or_else(
                            || {
                                AdapterFindingCode::Grammar
                                    .failure("imported header missing base fee")
                            },
                        )?),
                        withdrawals,
                        remaining_queue: call.remainingQueue,
                        outcomes,
                    },
                ));
                continue;
            }
        } else if events.iter().any(|event| {
            matches!(
                event,
                L1ProtocolEvent::Portal(
                    Portal::ZonePortalEvents::BatchSubmitted(_)
                        | Portal::ZonePortalEvents::WithdrawalProcessed(_)
                        | Portal::ZonePortalEvents::WithdrawalBounceBack(_)
                        | Portal::ZonePortalEvents::DepositBounceBack(_)
                        | Portal::ZonePortalEvents::DepositBounceBackPending(_)
                )
            )
        }) {
            return Err(AdapterFindingCode::Grammar
                .failure("direct-call event occurred outside its transaction envelope"));
        }
        let creation_token = if let Some(index) = creation_event_index {
            if index != 1
                || events
                    .iter()
                    .filter(|event| matches!(event, L1ProtocolEvent::FactoryZoneCreated(_)))
                    .count()
                    != 1
            {
                return Err(AdapterFindingCode::Grammar
                    .failure("creation requires TokenEnabled followed by one ZoneCreated"));
            }
            let L1ProtocolEvent::Portal(Portal::ZonePortalEvents::TokenEnabled(event)) = events[0]
            else {
                return Err(AdapterFindingCode::Grammar
                    .failure("creation requires TokenEnabled followed by one ZoneCreated"));
            };
            Some(TokenEnable {
                token: event.token,
                name: event.name.clone(),
                symbol: event.symbol.clone(),
                currency: event.currency.clone(),
            })
        } else {
            None
        };
        if !is_creation_block && is_creation_transaction {
            return Err(AdapterFindingCode::Grammar
                .failure("ZoneCreated occurred outside the configured creation block"));
        }
        for (event_index, event) in events.into_iter().enumerate() {
            match event {
                L1ProtocolEvent::FactoryZoneCreated(Factory::ZoneCreated {
                    portal,
                    zoneId,
                    initialToken,
                    ..
                }) if is_creation_block => {
                    operations.push(ImportedOperation::Create {
                        identity: PortalIdentity {
                            portal: *portal,
                            zone_id: *zoneId,
                            initial_token: *initialToken,
                        },
                        initial_token: creation_token.clone().ok_or_else(|| {
                            AdapterFindingCode::Grammar.failure("creation missing TokenEnabled")
                        })?,
                    });
                }
                L1ProtocolEvent::Portal(Portal::ZonePortalEvents::TokenEnabled(e))
                    if creation_event_index != Some(event_index + 1) =>
                {
                    operations.push(ImportedOperation::EnableToken(TokenEnable {
                        token: e.token,
                        name: e.name.clone(),
                        symbol: e.symbol.clone(),
                        currency: e.currency.clone(),
                    }))
                }
                L1ProtocolEvent::Portal(Portal::ZonePortalEvents::BouncebackGasUpdated(e)) => {
                    operations.push(ImportedOperation::UpdateBouncebackGas(e.bouncebackGas))
                }
                L1ProtocolEvent::Portal(Portal::ZonePortalEvents::DepositMade(e))
                    if direct_call.is_none_or(|call| call.as_process_withdrawals().is_none()) =>
                {
                    let d = ordinary_deposit_event(e, "deposit")?;
                    operations.push(ImportedOperation::AppendDeposit(d));
                    effects.push(Effect::DepositAppended {
                        id: crate::kernel::DepositId::new(
                            observation.portal_address(),
                            e.depositNumber,
                        )
                        .ok_or_else(|| {
                            AdapterFindingCode::Grammar.failure("zero deposit number")
                        })?,
                        queue_hash: e.newCurrentDepositQueueHash,
                    });
                }
                L1ProtocolEvent::Portal(Portal::ZonePortalEvents::RefundClaimed(e)) => {
                    operations.push(ImportedOperation::ClaimPortalRefund(RefundClaim {
                        token: e.token,
                        recipient: e.recipient,
                        amount: e.amount,
                    }));
                    effects.push(Effect::RefundClaimed {
                        token: e.token,
                        recipient: e.recipient,
                        amount: e.amount,
                    });
                }
                L1ProtocolEvent::Portal(Portal::ZonePortalEvents::BatchSubmitted(e)) => effects
                    .push(Effect::BatchSubmitted {
                        id: crate::kernel::BatchId::new(zone_id, e.withdrawalBatchIndex)
                            .ok_or_else(|| {
                                AdapterFindingCode::Grammar.failure("zero batch index")
                            })?,
                        queue_index: e.withdrawalQueueIndex,
                        processed_deposit_hash: e.nextProcessedDepositQueueHash,
                        final_block_hash: e.nextBlockHash,
                        queue_hash: e.withdrawalQueueHash,
                        processed_deposit_number: e.lastProcessedDepositNumber,
                    }),
                L1ProtocolEvent::Portal(Portal::ZonePortalEvents::TokenEnabled(_)) => {}
                L1ProtocolEvent::FactoryZoneCreated(_) => {}
                L1ProtocolEvent::KnownIgnored | L1ProtocolEvent::Portal(_) => {
                    return Err(AdapterFindingCode::Grammar
                        .failure("protocol event does not match the expected grammar"));
                }
            }
        }
    }
    Ok(ImportedAdaptation {
        facts: ImportedFacts {
            block_hash: observation.block_hash(),
            block_number: observation.block_number(),
            operations,
        },
        effects,
    })
}
