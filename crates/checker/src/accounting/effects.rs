//! Converts authenticated TIP-20 movements into account-ledger effects.

use alloy_primitives::U256;

use super::{AccountKey, BalanceChange, Effect};
use crate::{
    l1::{L1BlockEvidence, L1PortalEvent},
    l2::{
        DepositResult, L2BlockEvidence, L2BridgeAction, TokenTransfer, WithdrawalBounceBackStatus,
        WithdrawalOrigin,
    },
};

/// Convert canonical transfers after their protocol provenance has been authenticated.
fn from_transfers(transfers: impl IntoIterator<Item = TokenTransfer>) -> Vec<Effect> {
    transfers
        .into_iter()
        .filter_map(|transfer| {
            if transfer.amount.is_zero() || transfer.from == transfer.to {
                return None;
            }

            match (transfer.from.is_zero(), transfer.to.is_zero()) {
                (true, false) => Some(Effect::Credit {
                    key: AccountKey::new(transfer.token, transfer.to),
                    amount: transfer.amount,
                }),
                (false, true) => Some(Effect::Debit {
                    key: AccountKey::new(transfer.token, transfer.from),
                    amount: transfer.amount,
                }),
                (false, false) => Some(Effect::Transfer {
                    token: transfer.token,
                    from: transfer.from,
                    to: transfer.to,
                    amount: transfer.amount,
                }),
                (true, true) => None,
            }
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
    let mut effects = from_transfers(block.token_transfers());
    effects.extend(from_zone_actions(block.bridge_actions()));
    effects
}

/// Derive liability changes from authenticated Zone actions.
fn from_zone_actions<'a>(actions: impl Iterator<Item = &'a L2BridgeAction>) -> Vec<Effect> {
    let mut effects = Vec::new();
    for action in actions {
        match *action {
            L2BridgeAction::Deposit {
                token,
                amount,
                result: DepositResult::Processed { .. },
            } => effects.push(Effect::PendingDeposit {
                token,
                change: BalanceChange::Debit(amount),
            }),
            L2BridgeAction::WithdrawalRequested {
                origin: WithdrawalOrigin::User { .. },
                token,
                principal,
                ..
            } => effects.push(Effect::PendingWithdrawal {
                token,
                change: BalanceChange::Credit(principal),
            }),
            L2BridgeAction::WithdrawalBounceBack {
                token,
                amount,
                status: WithdrawalBounceBackStatus::Processed,
                ..
            } => {
                effects.push(Effect::PendingZoneRefund {
                    token,
                    change: BalanceChange::Debit(amount),
                });
            }
            L2BridgeAction::RefundClaimed { token, amount, .. } => {
                effects.push(Effect::PendingZoneRefund {
                    token,
                    change: BalanceChange::Debit(amount),
                });
            }
            L2BridgeAction::Deposit { .. }
            | L2BridgeAction::WithdrawalRequested { .. }
            | L2BridgeAction::WithdrawalBounceBack { .. } => {}
        }
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn zone_effects(transfers: &[TokenTransfer], actions: &[L2BridgeAction]) -> Vec<Effect> {
        let mut effects = from_transfers(transfers.iter().copied());
        effects.extend(from_zone_actions(actions.iter()));
        effects
    }

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
                TokenTransfer {
                    token,
                    from: Address::ZERO,
                    to: alice,
                    amount,
                },
                TokenTransfer {
                    token,
                    from: alice,
                    to: Address::ZERO,
                    amount,
                },
                TokenTransfer {
                    token,
                    from: alice,
                    to: bob,
                    amount,
                },
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
        let action = L2BridgeAction::WithdrawalRequested {
            withdrawal_index: 0,
            origin: WithdrawalOrigin::DepositBounceBack,
            token: Address::repeat_byte(1),
            principal: U256::from(10),
            fee: U256::ZERO,
        };

        assert!(from_zone_actions([&action].into_iter()).is_empty());
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

        let bounce_back = L2BridgeAction::WithdrawalBounceBack {
            recipient,
            token,
            amount,
            status: WithdrawalBounceBackStatus::Processed,
        };
        state
            .apply(&zone_effects(
                &[TokenTransfer {
                    token,
                    from: Address::ZERO,
                    to: recipient,
                    amount,
                }],
                &[bounce_back],
            ))
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

        let pending = L2BridgeAction::WithdrawalBounceBack {
            recipient,
            token,
            amount,
            status: WithdrawalBounceBackStatus::Pending,
        };
        state
            .apply(&from_zone_actions([&pending].into_iter()))
            .unwrap();
        let token_state = state.token(token).unwrap();
        assert_eq!(token_state.pending_zone_refunds, amount);
        assert_eq!(token_state.liability().unwrap(), amount);

        let claimed = L2BridgeAction::RefundClaimed {
            recipient,
            token,
            amount,
        };
        state
            .apply(&zone_effects(
                &[TokenTransfer {
                    token,
                    from: Address::ZERO,
                    to: recipient,
                    amount,
                }],
                &[claimed],
            ))
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
