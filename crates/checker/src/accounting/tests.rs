use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};

use super::{AccountKey, AccountingError, BalanceChange, Effect, LiabilityKind, State, TokenState};

fn address(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

fn state_with_tokens(tokens: impl IntoIterator<Item = Address>) -> State {
    let mut state = State::default();
    let effects = tokens
        .into_iter()
        .map(Effect::EnableToken)
        .collect::<Vec<_>>();
    state.apply(&effects).unwrap();
    state
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
    let mut state = state_with_tokens([token]);
    state
        .apply(&[Effect::Account {
            key: alice,
            change: BalanceChange::Credit(U256::MAX),
        }])
        .unwrap();
    let before = state.clone();

    let delta = state
        .apply(&[
            Effect::Account {
                key: alice,
                change: BalanceChange::Debit(U256::from(1)),
            },
            Effect::Account {
                key: bob,
                change: BalanceChange::Credit(U256::from(1)),
            },
        ])
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
            Effect::EnableToken(token),
            Effect::Account {
                key,
                change: BalanceChange::Credit(U256::from(10)),
            },
            Effect::Account {
                key,
                change: BalanceChange::Credit(U256::from(5)),
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

    let mut state = state_with_tokens([token_a, token_b, token_c]);
    state
        .apply(&[
            Effect::Account {
                key: AccountKey::new(token_a, account),
                change: BalanceChange::Credit(U256::from(100)),
            },
            Effect::Account {
                key: AccountKey::new(token_b, account),
                change: BalanceChange::Credit(U256::from(50)),
            },
            Effect::Account {
                key: AccountKey::new(token_c, account),
                change: BalanceChange::Credit(U256::from(200)),
            },
        ])
        .unwrap();
    let before = state.clone();

    // Touch only token_a and token_c; token_b's aggregate must stay valid
    // without appearing in this batch's delta at all.
    let delta = state
        .apply(&[
            Effect::Account {
                key: AccountKey::new(token_a, account),
                change: BalanceChange::Credit(U256::from(25)),
            },
            Effect::Account {
                key: AccountKey::new(token_c, account),
                change: BalanceChange::Credit(U256::from(75)),
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
    let mut state = state_with_tokens([key.token]);
    let before = state.clone();

    assert_eq!(
        state.apply(&[Effect::Account {
            key,
            change: BalanceChange::Debit(U256::from(1)),
        }]),
        Err(AccountingError::Underflow)
    );
    assert_eq!(state, before);
}

#[test]
fn checks_full_portal_liability() {
    let token = address(1);
    let mut state = state_with_tokens([token]);
    state
        .apply(&[
            Effect::Account {
                key: AccountKey::new(token, address(2)),
                change: BalanceChange::Credit(U256::from(100)),
            },
            Effect::Liability {
                token,
                kind: LiabilityKind::Deposit,
                change: BalanceChange::Credit(U256::from(20)),
            },
            Effect::Liability {
                token,
                kind: LiabilityKind::Withdrawal,
                change: BalanceChange::Credit(U256::from(30)),
            },
            Effect::Liability {
                token,
                kind: LiabilityKind::TempoRefund,
                change: BalanceChange::Credit(U256::from(5)),
            },
            Effect::Liability {
                token,
                kind: LiabilityKind::ZoneRefund,
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
    let mut state = state_with_tokens([token]);
    state
        .apply(&[Effect::Account {
            key,
            change: BalanceChange::Credit(U256::from(10)),
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

#[test]
fn rejects_unknown_token_changes_without_mutating_state() {
    let token = address(1);
    let effects = [
        Effect::Account {
            key: AccountKey::new(token, address(2)),
            change: BalanceChange::Credit(U256::from(1)),
        },
        Effect::Liability {
            token,
            kind: LiabilityKind::Deposit,
            change: BalanceChange::Credit(U256::from(1)),
        },
    ];

    for effect in effects {
        let mut state = State::default();
        assert_eq!(
            state.apply(&[effect]),
            Err(AccountingError::UnknownToken { token })
        );
        assert_eq!(state, State::default());
    }
}

#[test]
fn rejects_account_rows_for_unknown_tokens() {
    let token = address(1);
    let key = AccountKey::new(token, address(2));

    assert_eq!(
        State::from_rows([(key, U256::from(1))], []),
        Err(AccountingError::UnknownToken { token })
    );
}

#[test]
fn retains_zero_state_until_enablement_is_unwound() {
    let token = address(1);
    let key = AccountKey::new(token, address(2));
    let mut state = State::default();
    let enable = state.apply(&[Effect::EnableToken(token)]).unwrap();
    let balance = state
        .apply(&[
            Effect::Account {
                key,
                change: BalanceChange::Credit(U256::from(1)),
            },
            Effect::Account {
                key,
                change: BalanceChange::Debit(U256::from(1)),
            },
        ])
        .unwrap();

    assert_eq!(state.token(token), Some(TokenState::default()));
    state.unwind(balance).unwrap();
    assert_eq!(state.token(token), Some(TokenState::default()));
    state.unwind(enable).unwrap();
    assert_eq!(state.token(token), None);
}
