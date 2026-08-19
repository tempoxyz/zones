use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};

use super::{AccountKey, AccountingError, BalanceChange, Effect, State};

fn address(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

fn evidence(
    token: Address,
    total_supply: U256,
    balances: &[(Address, U256)],
) -> (Address, U256, BTreeMap<Address, U256>) {
    (token, total_supply, balances.iter().copied().collect())
}

#[test]
fn applies_transfers_and_unwinds_exactly() {
    let token = address(1);
    let alice = AccountKey::new(token, address(2));
    let bob = AccountKey::new(token, address(3));
    let mut state = State::default();
    state
        .apply(&[Effect::Credit {
            key: alice,
            amount: U256::from(100),
        }])
        .unwrap();
    let before = state.clone();

    let delta = state
        .apply(&[Effect::Transfer {
            token,
            from: alice.account,
            to: bob.account,
            amount: U256::from(40),
        }])
        .unwrap();
    state
        .verify_zone_state([evidence(
            token,
            U256::from(100),
            &[
                (alice.account, U256::from(60)),
                (bob.account, U256::from(40)),
            ],
        )])
        .unwrap();

    state.unwind(delta).unwrap();
    assert_eq!(state, before);
}

#[test]
fn apply_and_unwind_scope_aggregate_checks_to_touched_tokens() {
    let token_a = address(1);
    let token_b = address(2);
    let token_c = address(3);
    let account = address(10);

    let mut state = State::default();
    state
        .apply(&[
            Effect::Credit {
                key: AccountKey::new(token_a, account),
                amount: U256::from(100),
            },
            Effect::Credit {
                key: AccountKey::new(token_b, account),
                amount: U256::from(50),
            },
            Effect::Credit {
                key: AccountKey::new(token_c, account),
                amount: U256::from(200),
            },
        ])
        .unwrap();
    let before = state.clone();

    // Touch only token_a and token_c; token_b's aggregate must stay valid
    // without appearing in this batch's delta at all.
    let delta = state
        .apply(&[
            Effect::Credit {
                key: AccountKey::new(token_a, account),
                amount: U256::from(25),
            },
            Effect::Credit {
                key: AccountKey::new(token_c, account),
                amount: U256::from(75),
            },
        ])
        .unwrap();

    state
        .verify_zone_state([
            evidence(token_a, U256::from(125), &[(account, U256::from(125))]),
            evidence(token_b, U256::from(50), &[(account, U256::from(50))]),
            evidence(token_c, U256::from(275), &[(account, U256::from(275))]),
        ])
        .unwrap();

    state.unwind(delta).unwrap();
    assert_eq!(state, before);
}

#[test]
fn rejects_unbacked_debits_without_mutating_state() {
    let key = AccountKey::new(address(1), address(2));
    let mut state = State::default();
    let before = state.clone();

    assert_eq!(
        state.apply(&[Effect::Debit {
            key,
            amount: U256::from(1),
        }]),
        Err(AccountingError::Underflow)
    );
    assert_eq!(state, before);
}

#[test]
fn checks_full_portal_liability() {
    let token = address(1);
    let mut state = State::default();
    state
        .apply(&[
            Effect::Credit {
                key: AccountKey::new(token, address(2)),
                amount: U256::from(100),
            },
            Effect::PendingDeposit {
                token,
                change: BalanceChange::Credit(U256::from(20)),
            },
            Effect::PendingWithdrawal {
                token,
                change: BalanceChange::Credit(U256::from(30)),
            },
            Effect::PendingRefund {
                token,
                change: BalanceChange::Credit(U256::from(5)),
            },
        ])
        .unwrap();

    state
        .verify_portal_balances([(token, U256::from(155))])
        .unwrap();
    assert!(matches!(
        state.verify_portal_balances([(token, U256::from(154))]),
        Err(AccountingError::CollateralShortfall { .. })
    ));
}

#[test]
fn detects_balance_and_supply_mismatches() {
    let token = address(1);
    let key = AccountKey::new(token, address(2));
    let mut state = State::default();
    state
        .apply(&[Effect::Credit {
            key,
            amount: U256::from(10),
        }])
        .unwrap();

    assert!(matches!(
        state.verify_zone_state([evidence(
            token,
            U256::from(10),
            &[(key.account, U256::from(9))]
        )]),
        Err(AccountingError::BalanceMismatch { .. })
    ));
    assert!(matches!(
        state.verify_zone_state([evidence(
            token,
            U256::from(11),
            &[(key.account, U256::from(10))]
        )]),
        Err(AccountingError::SupplyMismatch { .. })
    ));
}
