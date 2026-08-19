//! Converts authenticated TIP-20 movements into account-ledger effects.

use alloy_primitives::{Address, U256};

use super::{AccountKey, BalanceChange, Effect};
use crate::{
    l1::{L1BlockEvidence, L1PortalEvent},
    l2::{L2BlockEvidence, L2BridgeEvent},
};

/// Convert canonical transfers after their protocol provenance has been authenticated.
fn from_transfers(
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

/// Derive liability changes from one authenticated Tempo block.
pub(crate) fn from_tempo(block: &L1BlockEvidence) -> Vec<Effect> {
    from_tempo_events(block.portal_events())
}

/// Derive liability changes from authenticated Tempo Portal events.
fn from_tempo_events<'a>(events: impl IntoIterator<Item = &'a L1PortalEvent>) -> Vec<Effect> {
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
                change: BalanceChange::Credit(U256::from(net_amount)),
            }),
            L1PortalEvent::WithdrawalProcessed {
                token,
                amount,
                callback_success: true,
                ..
            } => {
                effects.push(Effect::PendingWithdrawal {
                    token,
                    change: BalanceChange::Debit(U256::from(amount)),
                });
            }
            L1PortalEvent::DepositBounceBack {
                token,
                amount,
                bounceback_fee,
            } => effects.push(Effect::PendingDeposit {
                token,
                change: BalanceChange::Debit(U256::from(amount) + U256::from(bounceback_fee)),
            }),
            L1PortalEvent::DepositBounceBackPending {
                token,
                amount,
                bounceback_fee,
            } => {
                effects.push(Effect::PendingDeposit {
                    token,
                    change: BalanceChange::Debit(U256::from(amount) + U256::from(bounceback_fee)),
                });
                effects.push(Effect::PendingTempoRefund {
                    token,
                    change: BalanceChange::Credit(U256::from(amount)),
                });
            }
            L1PortalEvent::RefundClaimed { token, amount, .. } => {
                effects.push(Effect::PendingTempoRefund {
                    token,
                    change: BalanceChange::Debit(U256::from(amount)),
                });
            }
            L1PortalEvent::WithdrawalBounceBack { token, amount } => {
                let amount = U256::from(amount);
                effects.push(Effect::PendingWithdrawal {
                    token,
                    change: BalanceChange::Debit(amount),
                });
                effects.push(Effect::PendingZoneRefund {
                    token,
                    change: BalanceChange::Credit(amount),
                });
            }
            L1PortalEvent::WithdrawalProcessed { .. } => {}
        }
    }
    effects
}

/// Derive account and liability changes from authenticated Zone evidence.
pub(crate) fn from_zone(block: &L2BlockEvidence) -> Vec<Effect> {
    from_zone_events(block.bridge_events())
}

/// Derive account and liability changes from authenticated Zone events.
fn from_zone_events<'a>(events: impl Iterator<Item = &'a L2BridgeEvent> + Clone) -> Vec<Effect> {
    let mut effects = from_transfers(events.clone().filter_map(|event| match *event {
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
                change: BalanceChange::Debit(U256::from(amount)),
            }),
            L2BridgeEvent::WithdrawalRequested {
                sender,
                token,
                principal,
                ..
            } if !sender.is_zero() => effects.push(Effect::PendingWithdrawal {
                token,
                change: BalanceChange::Credit(U256::from(principal)),
            }),
            L2BridgeEvent::WithdrawalBounceBack {
                token,
                amount,
                processed: true,
                ..
            } => {
                effects.push(Effect::PendingZoneRefund {
                    token,
                    change: BalanceChange::Debit(U256::from(amount)),
                });
            }
            L2BridgeEvent::RefundClaimed { token, amount, .. } => {
                effects.push(Effect::PendingZoneRefund {
                    token,
                    change: BalanceChange::Debit(U256::from(amount)),
                });
            }
            L2BridgeEvent::TempoAdvanced(_)
            | L2BridgeEvent::DepositOutcome { .. }
            | L2BridgeEvent::WithdrawalRequested { .. }
            | L2BridgeEvent::WithdrawalBounceBack { .. }
            | L2BridgeEvent::Transfer { .. }
            | L2BridgeEvent::TokenBurn { .. } => {}
        }
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_token(token: Address) -> crate::accounting::State {
        let mut state = crate::accounting::State::default();
        state.apply(&[Effect::EnableToken { token }]).unwrap();
        state
    }

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
    fn deposit_bounce_back_does_not_create_withdrawal_liability() {
        let event = L2BridgeEvent::WithdrawalRequested {
            withdrawal_index: 0,
            sender: Address::ZERO,
            token: Address::repeat_byte(1),
            principal: 10,
            fee: 0,
        };

        assert!(from_zone_events([&event].into_iter()).is_empty());
    }

    #[test]
    fn failed_withdrawal_returns_through_zone_bounce_back() {
        let token = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);
        let amount = U256::from(10);
        let mut state = state_with_token(token);
        state
            .apply(&[Effect::PendingWithdrawal {
                token,
                change: BalanceChange::Credit(amount),
            }])
            .unwrap();
        let enqueued = L1PortalEvent::WithdrawalBounceBack { token, amount: 10 };
        let failed = L1PortalEvent::WithdrawalProcessed {
            to: recipient,
            token,
            amount: 10,
            callback_success: false,
        };
        state
            .apply(&from_tempo_events([&enqueued, &failed]))
            .unwrap();
        let token_state = state.token(token).unwrap();
        assert_eq!(token_state.pending_withdrawals, U256::ZERO);
        assert_eq!(token_state.pending_zone_refunds, amount);
        assert_eq!(token_state.liability().unwrap(), amount);

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
            .apply(&from_zone_events([&mint, &bounce_back].into_iter()))
            .unwrap();

        let token_state = state.token(token).unwrap();
        assert_eq!(token_state.pending_zone_refunds, U256::ZERO);
        assert_eq!(token_state.account_total, amount);
        assert_eq!(token_state.liability().unwrap(), amount);
        assert_eq!(
            state.account(AccountKey::new(token, recipient)),
            Some(amount)
        );
    }

    #[test]
    fn pending_zone_refund_stays_liability_until_claimed() {
        let token = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);
        let amount = U256::from(10);
        let mut state = state_with_token(token);
        state
            .apply(&[Effect::PendingWithdrawal {
                token,
                change: BalanceChange::Credit(amount),
            }])
            .unwrap();
        let enqueued = L1PortalEvent::WithdrawalBounceBack { token, amount: 10 };
        let failed = L1PortalEvent::WithdrawalProcessed {
            to: recipient,
            token,
            amount: 10,
            callback_success: false,
        };
        state
            .apply(&from_tempo_events([&enqueued, &failed]))
            .unwrap();

        let pending = L2BridgeEvent::WithdrawalBounceBack {
            recipient,
            token,
            amount: 10,
            processed: false,
        };
        state
            .apply(&from_zone_events([&pending].into_iter()))
            .unwrap();
        let token_state = state.token(token).unwrap();
        assert_eq!(token_state.pending_zone_refunds, amount);
        assert_eq!(token_state.liability().unwrap(), amount);

        let mint = L2BridgeEvent::Transfer {
            token,
            from: Address::ZERO,
            to: recipient,
            amount,
        };
        let claimed = L2BridgeEvent::RefundClaimed {
            recipient,
            token,
            amount: 10,
        };
        state
            .apply(&from_zone_events([&mint, &claimed].into_iter()))
            .unwrap();

        let token_state = state.token(token).unwrap();
        assert_eq!(token_state.pending_zone_refunds, U256::ZERO);
        assert_eq!(token_state.account_total, amount);
        assert_eq!(token_state.liability().unwrap(), amount);
    }

    #[test]
    fn tempo_refund_replaces_failed_deposit_liability_until_claimed() {
        let token = Address::repeat_byte(1);
        let amount = U256::from(10);
        let fee = U256::from(1);
        let mut state = crate::accounting::State::default();
        state
            .apply(&[
                Effect::EnableToken { token },
                Effect::PendingDeposit {
                    token,
                    change: BalanceChange::Credit(amount + fee),
                },
            ])
            .unwrap();

        let pending = L1PortalEvent::DepositBounceBackPending {
            token,
            amount: 10,
            bounceback_fee: 1,
        };
        state.apply(&from_tempo_events([&pending])).unwrap();
        let token_state = state.token(token).unwrap();
        assert_eq!(token_state.pending_deposits, U256::ZERO);
        assert_eq!(token_state.pending_tempo_refunds, amount);
        assert_eq!(token_state.liability().unwrap(), amount);

        let claimed = L1PortalEvent::RefundClaimed {
            recipient: Address::repeat_byte(2),
            token,
            amount: 10,
        };
        state.apply(&from_tempo_events([&claimed])).unwrap();
        let token_state = state.token(token).unwrap();
        assert_eq!(token_state.pending_tempo_refunds, U256::ZERO);
        assert_eq!(token_state.liability().unwrap(), U256::ZERO);
    }
}
