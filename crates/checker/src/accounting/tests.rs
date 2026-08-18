use alloy_primitives::{Address, U256};

use super::{AccountKey, AccountingError, Effect, State};

fn address(byte: u8) -> Address {
    Address::repeat_byte(byte)
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
        .verify_zone_state(
            [(alice, U256::from(60)), (bob, U256::from(40))],
            [(token, U256::from(100))],
        )
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
                amount: U256::from(20),
                increase: true,
            },
            Effect::PendingWithdrawal {
                token,
                amount: U256::from(30),
                increase: true,
            },
            Effect::PendingRefund {
                token,
                amount: U256::from(5),
                increase: true,
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
        state.verify_zone_state([(key, U256::from(9))], [(token, U256::from(10))]),
        Err(AccountingError::BalanceMismatch { .. })
    ));
    assert!(matches!(
        state.verify_zone_state([(key, U256::from(10))], [(token, U256::from(11))]),
        Err(AccountingError::SupplyMismatch { .. })
    ));
}
