//! Converts authenticated TIP-20 movements into account-ledger effects.

use alloy_primitives::{Address, U256};

use super::{AccountKey, Effect};
use crate::{
    l1::{L1BlockEvidence, L1PortalEvent},
    l2::L2BridgeEvent,
};

/// Convert canonical transfers after their protocol provenance has been authenticated.
pub(crate) fn from_transfers(
    transfers: impl IntoIterator<Item = (Address, Address, Address, U256)>,
) -> Vec<Effect> {
    transfers
        .into_iter()
        .filter_map(|(token, from, to, amount)| {
            if amount.is_zero() || from == to {
                return None;
            }
            Some(if from.is_zero() {
                Effect::Credit {
                    key: AccountKey::new(token, to),
                    amount,
                }
            } else if to.is_zero() {
                Effect::Debit {
                    key: AccountKey::new(token, from),
                    amount,
                }
            } else {
                Effect::Transfer {
                    token,
                    from,
                    to,
                    amount,
                }
            })
        })
        .collect()
}

/// Derive liability changes from a contiguous span of authenticated Tempo history.
pub(crate) fn from_tempo_history(history: &[L1BlockEvidence]) -> Vec<Effect> {
    history
        .iter()
        .flat_map(|block| from_tempo(block.portal_events()))
        .collect()
}

/// Derive liability changes from authenticated Tempo Portal events.
pub(crate) fn from_tempo<'a>(events: impl IntoIterator<Item = &'a L1PortalEvent>) -> Vec<Effect> {
    let mut effects = Vec::new();
    for event in events {
        match *event {
            L1PortalEvent::TokenEnabled { token } => {
                effects.push(Effect::EnableToken { token });
            }
            L1PortalEvent::DepositMade {
                token, net_amount, ..
            } => effects.push(Effect::PendingDeposit {
                token,
                amount: U256::from(net_amount),
                increase: true,
            }),
            L1PortalEvent::WithdrawalProcessed {
                token,
                amount,
                callback_success: true,
                ..
            } => {
                effects.push(Effect::PendingWithdrawal {
                    token,
                    amount: U256::from(amount),
                    increase: false,
                });
            }
            L1PortalEvent::DepositBounceBack {
                token,
                amount,
                bounceback_fee,
                ..
            } => effects.push(Effect::PendingDeposit {
                token,
                amount: U256::from(amount) + U256::from(bounceback_fee),
                increase: false,
            }),
            L1PortalEvent::DepositBounceBackPending {
                token,
                amount,
                bounceback_fee,
                ..
            } => {
                effects.push(Effect::PendingDeposit {
                    token,
                    amount: U256::from(amount) + U256::from(bounceback_fee),
                    increase: false,
                });
                effects.push(Effect::PendingRefund {
                    token,
                    amount: U256::from(amount),
                    increase: true,
                });
            }
            L1PortalEvent::RefundClaimed { token, amount, .. } => {
                effects.push(Effect::PendingRefund {
                    token,
                    amount: U256::from(amount),
                    increase: false,
                });
            }
            L1PortalEvent::BatchSubmitted { .. }
            | L1PortalEvent::WithdrawalProcessed { .. }
            | L1PortalEvent::WithdrawalBounceBack { .. } => {}
        }
    }
    effects
}

/// Derive account and liability changes from authenticated Zone events.
pub(crate) fn from_zone<'a>(
    events: impl IntoIterator<Item = &'a L2BridgeEvent>,
) -> Result<Vec<Effect>, EffectError> {
    let events = events.into_iter().collect::<Vec<_>>();
    authenticate_mints(&events)?;
    let mut effects = from_transfers(events.iter().filter_map(|event| match **event {
        L2BridgeEvent::Transfer {
            token,
            from,
            to,
            amount,
        } => Some((token, from, to, amount)),
        _ => None,
    }));
    for event in events {
        match *event {
            L2BridgeEvent::DepositOutcome {
                token,
                amount,
                processed: true,
                ..
            } => effects.push(Effect::PendingDeposit {
                token,
                amount: U256::from(amount),
                increase: false,
            }),
            L2BridgeEvent::WithdrawalRequested {
                token,
                principal,
                is_deposit_bounce_back: false,
                ..
            } => effects.push(Effect::PendingWithdrawal {
                token,
                amount: U256::from(principal),
                increase: true,
            }),
            L2BridgeEvent::WithdrawalBounceBack {
                token,
                amount,
                processed: true,
                ..
            }
            | L2BridgeEvent::RefundClaimed { token, amount, .. } => {
                effects.push(Effect::PendingWithdrawal {
                    token,
                    amount: U256::from(amount),
                    increase: false,
                });
            }
            L2BridgeEvent::TempoAdvanced(_)
            | L2BridgeEvent::DepositOutcome { .. }
            | L2BridgeEvent::WithdrawalRequested { .. }
            | L2BridgeEvent::WithdrawalBounceBack { .. }
            | L2BridgeEvent::Transfer { .. }
            | L2BridgeEvent::TokenBurn { .. }
            | L2BridgeEvent::BatchFinalized { .. } => {}
        }
    }
    Ok(effects)
}

/// Require every Inbox mint to name the recipient authenticated by its bridge event.
fn authenticate_mints(events: &[&L2BridgeEvent]) -> Result<(), EffectError> {
    let mut mints = events
        .iter()
        .filter_map(|event| match **event {
            L2BridgeEvent::Transfer {
                token,
                from,
                to,
                amount,
            } if from.is_zero() => Some((token, to, amount)),
            _ => None,
        })
        .collect::<Vec<_>>();
    for expected in events.iter().filter_map(|event| match **event {
        L2BridgeEvent::DepositOutcome {
            recipient: Some(recipient),
            token,
            amount,
            processed: true,
            ..
        }
        | L2BridgeEvent::WithdrawalBounceBack {
            recipient,
            token,
            amount,
            processed: true,
        }
        | L2BridgeEvent::RefundClaimed {
            recipient,
            token,
            amount,
        } => Some((token, recipient, U256::from(amount))),
        _ => None,
    }) {
        let Some(index) = mints.iter().position(|mint| *mint == expected) else {
            return Err(EffectError::MissingMint {
                token: expected.0,
                recipient: expected.1,
                amount: expected.2,
            });
        };
        mints.swap_remove(index);
    }
    if let Some((token, recipient, amount)) = mints.into_iter().next() {
        return Err(EffectError::UnexpectedMint {
            token,
            recipient,
            amount,
        });
    }
    Ok(())
}

/// Bridge-event evidence that could not be reconciled into effects.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EffectError {
    #[error("missing authenticated mint of {amount} {token} to {recipient}")]
    MissingMint {
        token: Address,
        recipient: Address,
        amount: U256,
    },
    #[error("unexpected mint of {amount} {token} to {recipient}")]
    UnexpectedMint {
        token: Address,
        recipient: Address,
        amount: U256,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_mints_burns_and_transfers() {
        let token = Address::repeat_byte(1);
        let alice = Address::repeat_byte(2);
        let bob = Address::repeat_byte(3);
        let amount = U256::from(10);

        assert_eq!(
            from_transfers([
                (token, Address::ZERO, alice, amount),
                (token, alice, Address::ZERO, amount),
                (token, alice, bob, amount),
            ]),
            vec![
                Effect::Credit {
                    key: AccountKey::new(token, alice),
                    amount,
                },
                Effect::Debit {
                    key: AccountKey::new(token, alice),
                    amount,
                },
                Effect::Transfer {
                    token,
                    from: alice,
                    to: bob,
                    amount,
                },
            ]
        );
    }

    #[test]
    fn authenticates_encrypted_deposit_recipient() {
        let token = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);
        let amount = 10;
        let outcome = L2BridgeEvent::DepositOutcome {
            deposit_hash: Default::default(),
            recipient: Some(recipient),
            token,
            amount,
            processed: true,
        };
        let mint = L2BridgeEvent::Transfer {
            token,
            from: Address::ZERO,
            to: recipient,
            amount: U256::from(amount),
        };
        assert!(from_zone([&outcome, &mint]).is_ok());

        let wrong_mint = L2BridgeEvent::Transfer {
            token,
            from: Address::ZERO,
            to: Address::repeat_byte(3),
            amount: U256::from(amount),
        };
        assert!(from_zone([&outcome, &wrong_mint]).is_err());
    }

    #[test]
    fn rejects_unexplained_mint() {
        let mint = L2BridgeEvent::Transfer {
            token: Address::repeat_byte(1),
            from: Address::ZERO,
            to: Address::repeat_byte(2),
            amount: U256::from(10),
        };
        assert!(from_zone([&mint]).is_err());
    }

    #[test]
    fn deposit_bounce_back_does_not_create_withdrawal_liability() {
        let event = L2BridgeEvent::WithdrawalRequested {
            withdrawal_index: 0,
            sender: Address::ZERO,
            token: Address::repeat_byte(1),
            principal: 10,
            fee: 0,
            fallback_nonce: 0,
            is_deposit_bounce_back: true,
        };

        assert!(from_zone([&event]).unwrap().is_empty());
    }

    #[test]
    fn failed_withdrawal_stays_pending_until_zone_bounce_back() {
        let token = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);
        let amount = U256::from(10);
        let mut state = crate::accounting::State::default();
        state
            .apply(&[Effect::PendingWithdrawal {
                token,
                amount,
                increase: true,
            }])
            .unwrap();
        let failed = L1PortalEvent::WithdrawalProcessed {
            to: recipient,
            sender_tag: Default::default(),
            token,
            amount: 10,
            callback_success: false,
        };
        state.apply(&from_tempo([&failed])).unwrap();
        assert_eq!(state.token(token).unwrap().pending_withdrawals, amount);

        let mint = L2BridgeEvent::Transfer {
            token,
            from: Address::ZERO,
            to: recipient,
            amount,
        };
        let bounce_back = L2BridgeEvent::WithdrawalBounceBack {
            recipient,
            token,
            amount: 10,
            processed: true,
        };
        state
            .apply(&from_zone([&mint, &bounce_back]).unwrap())
            .unwrap();

        let token_state = state.token(token).unwrap();
        assert_eq!(token_state.pending_withdrawals, U256::ZERO);
        assert_eq!(token_state.account_total, amount);
        assert_eq!(
            state.account(AccountKey::new(token, recipient)),
            Some(amount)
        );
    }
}
