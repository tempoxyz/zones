//! Parses the exact Portal event grammar for withdrawal processing.

use alloy_primitives::Address;

use crate::{
    failure::Failure,
    kernel::{Effect, PortalCallbackOperation, RefundClaim, TokenEnable, WithdrawalOutcome},
    observe::events::{L1ProtocolEvent, Portal},
};

use super::super::{AdapterFindingCode, deposits::ordinary_deposit_event};

/// Parsed effects and outcomes for one `processWithdrawals` call.
pub(crate) struct WithdrawalAdaptation {
    pub(crate) outcomes: Vec<WithdrawalOutcome>,
    pub(crate) effects: Vec<Effect>,
}

/// Parse the exact event sequence produced by `processWithdrawals`.
#[cfg(test)]
pub(super) fn parse_withdrawal_events(
    events: &[&L1ProtocolEvent],
    member_count: usize,
    portal: Address,
) -> Result<WithdrawalAdaptation, Failure> {
    let (adaptation, consumed) = parse_withdrawal_events_prefix(events, member_count, portal)?;
    if consumed != events.len() {
        return Err(AdapterFindingCode::Grammar
            .failure("processWithdrawals has extra or out-of-order events"));
    }
    Ok(adaptation)
}

/// Parse one `processWithdrawals` prefix and return the consumed event count.
pub(super) fn parse_withdrawal_events_prefix(
    events: &[&L1ProtocolEvent],
    member_count: usize,
    portal: Address,
) -> Result<(WithdrawalAdaptation, usize), Failure> {
    let mut cursor = 0;
    let mut outcomes = Vec::with_capacity(member_count);
    let mut effects = Vec::new();
    for _ in 0..member_count {
        let mut operations = Vec::new();
        while let Some(event) = events.get(cursor).copied() {
            let Some((operation, effect)) = parse_callback_operation(event, portal)? else {
                break;
            };
            cursor += 1;
            operations.push(operation);
            if let Some(effect) = effect {
                effects.push(effect);
            }
        }
        let event = events.get(cursor).ok_or_else(|| {
            AdapterFindingCode::Grammar.failure("processWithdrawals missing member outcome")
        })?;
        cursor += 1;
        if !operations.is_empty()
            && !matches!(
                event,
                L1ProtocolEvent::Portal(Portal::ZonePortalEvents::WithdrawalProcessed(_))
            )
        {
            return Err(AdapterFindingCode::Grammar
                .failure("callback operations must be followed by WithdrawalProcessed"));
        }
        match event {
            L1ProtocolEvent::Portal(Portal::ZonePortalEvents::DepositBounceBack(e)) => {
                outcomes.push(WithdrawalOutcome::FailedDepositPaid {
                    collected_fee: e.bouncebackFee,
                });
                effects.push(Effect::FailedDepositRefunded {
                    recipient: e.tempoRefundRecipient,
                    token: e.token,
                    amount: e.amount,
                    fee: e.bouncebackFee,
                    pending: false,
                });
            }
            L1ProtocolEvent::Portal(Portal::ZonePortalEvents::DepositBounceBackPending(e)) => {
                outcomes.push(WithdrawalOutcome::FailedDepositPending {
                    collected_fee: e.bouncebackFee,
                });
                effects.push(Effect::FailedDepositRefunded {
                    recipient: e.tempoRefundRecipient,
                    token: e.token,
                    amount: e.amount,
                    fee: e.bouncebackFee,
                    pending: true,
                });
            }
            L1ProtocolEvent::Portal(Portal::ZonePortalEvents::WithdrawalBounceBack(e)) => {
                effects.push(Effect::BounceBackAppended {
                    fallback_nonce: e.fallbackNonce,
                    token: e.token,
                    amount: e.amount,
                    id: crate::kernel::DepositId::new(portal, e.depositNumber).ok_or_else(
                        || AdapterFindingCode::Grammar.failure("zero bounceback deposit number"),
                    )?,
                    queue_hash: e.newCurrentDepositQueueHash,
                });
                let Some(L1ProtocolEvent::Portal(Portal::ZonePortalEvents::WithdrawalProcessed(
                    processed,
                ))) = events.get(cursor).copied()
                else {
                    return Err(AdapterFindingCode::Grammar
                        .failure("WithdrawalBounceBack must be followed by WithdrawalProcessed"));
                };
                cursor += 1;
                if processed.callbackSuccess {
                    return Err(AdapterFindingCode::Grammar
                        .failure("bounce WithdrawalProcessed callbackSuccess must be false"));
                }
                effects.push(Effect::UserWithdrawalProcessed {
                    to: processed.to,
                    sender_tag: processed.senderTag,
                    token: processed.token,
                    amount: processed.amount,
                    callback_success: false,
                });
                outcomes.push(WithdrawalOutcome::UserBounced);
            }
            L1ProtocolEvent::Portal(Portal::ZonePortalEvents::WithdrawalProcessed(processed)) => {
                if !processed.callbackSuccess {
                    return Err(AdapterFindingCode::Grammar
                        .failure("delivered WithdrawalProcessed callbackSuccess must be true"));
                }
                effects.push(Effect::UserWithdrawalProcessed {
                    to: processed.to,
                    sender_tag: processed.senderTag,
                    token: processed.token,
                    amount: processed.amount,
                    callback_success: true,
                });
                outcomes.push(WithdrawalOutcome::UserDelivered { operations });
            }
            _ => {
                return Err(AdapterFindingCode::Grammar
                    .failure("unexpected processWithdrawals member event"));
            }
        }
    }
    Ok((WithdrawalAdaptation { outcomes, effects }, cursor))
}

/// Parse one checker-relevant Portal event emitted by a withdrawal callback.
fn parse_callback_operation(
    event: &L1ProtocolEvent,
    portal: Address,
) -> Result<Option<(PortalCallbackOperation, Option<Effect>)>, Failure> {
    let operation = match event {
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::DepositMade(event)) => {
            let deposit = ordinary_deposit_event(event, "callback")?;
            let effect = Effect::DepositAppended {
                id: crate::kernel::DepositId::new(portal, event.depositNumber).ok_or_else(
                    || AdapterFindingCode::Grammar.failure("zero callback deposit number"),
                )?,
                queue_hash: event.newCurrentDepositQueueHash,
            };
            (
                PortalCallbackOperation::AppendDeposit(deposit),
                Some(effect),
            )
        }
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::RefundClaimed(event)) => {
            let claim = RefundClaim {
                token: event.token,
                recipient: event.recipient,
                amount: event.amount,
            };
            let effect = Effect::RefundClaimed {
                token: event.token,
                recipient: event.recipient,
                amount: event.amount,
            };
            (PortalCallbackOperation::ClaimRefund(claim), Some(effect))
        }
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::TokenEnabled(event)) => (
            PortalCallbackOperation::EnableToken(TokenEnable {
                token: event.token,
                name: event.name.clone(),
                symbol: event.symbol.clone(),
                currency: event.currency.clone(),
            }),
            None,
        ),
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::BouncebackGasUpdated(event)) => (
            PortalCallbackOperation::UpdateBouncebackGas(event.bouncebackGas),
            None,
        ),
        _ => return Ok(None),
    };
    Ok(Some(operation))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256};

    use super::*;

    const PORTAL_ADDRESS: Address = Address::repeat_byte(0x11);

    fn refund_claimed() -> L1ProtocolEvent {
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::RefundClaimed(
            Portal::RefundClaimed {
                recipient: Address::repeat_byte(0x22),
                token: Address::repeat_byte(0x33),
                amount: 44,
            },
        ))
    }

    fn withdrawal_processed(callback_success: bool) -> L1ProtocolEvent {
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::WithdrawalProcessed(
            Portal::WithdrawalProcessed {
                to: Address::repeat_byte(0x44),
                senderTag: B256::repeat_byte(0x55),
                token: Address::repeat_byte(0x33),
                amount: 66,
                callbackSuccess: callback_success,
            },
        ))
    }

    fn deposit_made() -> L1ProtocolEvent {
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::DepositMade(Portal::DepositMade {
            newCurrentDepositQueueHash: B256::repeat_byte(0x66),
            sender: Address::repeat_byte(0x77),
            token: Address::repeat_byte(0x33),
            netAmount: 88,
            fee: 9,
            keyIndex: U256::from(10),
            ephemeralPubkeyX: B256::repeat_byte(0x88),
            ephemeralPubkeyYParity: 2,
            ciphertext: Bytes::from(vec![0; 64]),
            nonce: FixedBytes::ZERO,
            tag: FixedBytes::ZERO,
            tempoRefundRecipient: Address::repeat_byte(0x99),
            depositNumber: 1,
        }))
    }

    fn token_enabled() -> L1ProtocolEvent {
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::TokenEnabled(
            Portal::TokenEnabled {
                token: Address::repeat_byte(0xaa),
                name: "Callback Token".into(),
                symbol: "CBK".into(),
                currency: "USD".into(),
            },
        ))
    }

    fn bounceback_gas_updated() -> L1ProtocolEvent {
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::BouncebackGasUpdated(
            Portal::BouncebackGasUpdated { bouncebackGas: 11 },
        ))
    }

    #[test]
    fn callback_refund_claim_precedes_successful_withdrawal_completion() {
        let events = [refund_claimed(), withdrawal_processed(true)];
        let events = events.iter().collect::<Vec<_>>();

        let adaptation = parse_withdrawal_events(&events, 1, PORTAL_ADDRESS).unwrap();

        assert_eq!(
            adaptation.outcomes,
            vec![WithdrawalOutcome::UserDelivered {
                operations: vec![PortalCallbackOperation::ClaimRefund(RefundClaim {
                    token: Address::repeat_byte(0x33),
                    recipient: Address::repeat_byte(0x22),
                    amount: 44,
                })],
            }]
        );
        assert_eq!(
            adaptation.effects,
            vec![
                Effect::RefundClaimed {
                    token: Address::repeat_byte(0x33),
                    recipient: Address::repeat_byte(0x22),
                    amount: 44,
                },
                Effect::UserWithdrawalProcessed {
                    to: Address::repeat_byte(0x44),
                    sender_tag: B256::repeat_byte(0x55),
                    token: Address::repeat_byte(0x33),
                    amount: 66,
                    callback_success: true,
                },
            ]
        );
    }

    #[test]
    fn mixed_callback_operations_preserve_receipt_order() {
        let events = [
            deposit_made(),
            refund_claimed(),
            token_enabled(),
            bounceback_gas_updated(),
            withdrawal_processed(true),
        ];
        let events = events.iter().collect::<Vec<_>>();

        let adaptation = parse_withdrawal_events(&events, 1, PORTAL_ADDRESS).unwrap();

        let [WithdrawalOutcome::UserDelivered { operations }] = adaptation.outcomes.as_slice()
        else {
            panic!("expected one delivered withdrawal")
        };
        assert!(matches!(
            operations.as_slice(),
            [
                PortalCallbackOperation::AppendDeposit(_),
                PortalCallbackOperation::ClaimRefund(_),
                PortalCallbackOperation::EnableToken(TokenEnable { token, .. }),
                PortalCallbackOperation::UpdateBouncebackGas(11),
            ] if *token == Address::repeat_byte(0xaa)
        ));
        assert!(matches!(
            adaptation.effects.as_slice(),
            [
                Effect::DepositAppended { .. },
                Effect::RefundClaimed { .. },
                Effect::UserWithdrawalProcessed {
                    callback_success: true,
                    ..
                },
            ]
        ));
    }

    #[test]
    fn callback_operation_cannot_precede_a_failed_withdrawal_completion() {
        let events = [refund_claimed(), withdrawal_processed(false)];
        let events = events.iter().collect::<Vec<_>>();

        assert!(parse_withdrawal_events(&events, 1, PORTAL_ADDRESS).is_err());
    }
}
