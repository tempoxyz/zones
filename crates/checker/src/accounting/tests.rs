use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};

use super::{AccountKey, AccountingError, BalanceChange, Effect, State, TokenState};

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
    let alice = AccountKey::new(token, address(3));
    let bob = AccountKey::new(token, address(2));
    let mut state = State::default();
    state
        .apply(&[Effect::Credit {
            key: alice,
            amount: U256::MAX,
        }])
        .unwrap();
    let before = state.clone();

    let delta = state
        .apply(&[Effect::Transfer {
            token,
            from: alice.account,
            to: bob.account,
            amount: U256::from(1),
        }])
        .unwrap();
    state
        .verify_zone_state([evidence(
            token,
            U256::MAX,
            &[
                (alice.account, U256::MAX - U256::from(1)),
                (bob.account, U256::from(1)),
            ],
        )])
        .unwrap();

    state.unwind(delta).unwrap();
    assert_eq!(state, before);
}

#[test]
fn records_each_changed_row_once() {
    let token = address(1);
    let key = AccountKey::new(token, address(2));
    let mut state = State::default();

    let delta = state
        .apply(&[
            Effect::Credit {
                key,
                amount: U256::from(10),
            },
            Effect::Credit {
                key,
                amount: U256::from(5),
            },
        ])
        .unwrap();

    assert_eq!(delta.accounts, vec![(key, None)]);
    assert_eq!(delta.tokens, vec![(token, None)]);
    assert_eq!(state.account(key), Some(U256::from(15)));
    assert_eq!(state.token(token).unwrap().account_total, U256::from(15));
}

#[test]
fn rejects_inconsistent_changed_aggregate() {
    let token = address(1);
    let key = AccountKey::new(token, address(2));
    let state = State {
        accounts: [(key, U256::from(10))].into(),
        tokens: [(
            token,
            TokenState {
                account_total: U256::from(11),
                ..Default::default()
            },
        )]
        .into(),
    };
    let accounts = [(key, None)].into();
    let tokens = [(token, None)].into();

    assert_eq!(
        state.validate_changed_aggregates(&accounts, &tokens),
        Err(AccountingError::AggregateMismatch {
            token,
            expected: U256::from(10),
            actual: U256::from(11),
        })
    );
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
            Effect::PendingTempoRefund {
                token,
                change: BalanceChange::Credit(U256::from(5)),
            },
            Effect::PendingZoneRefund {
                token,
                change: BalanceChange::Credit(U256::from(7)),
            },
        ])
        .unwrap();

    state
        .verify_portal_balances([(token, U256::from(162))])
        .unwrap();
    assert!(matches!(
        state.verify_portal_balances([(token, U256::from(161))]),
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
