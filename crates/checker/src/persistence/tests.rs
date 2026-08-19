use alloy_primitives::{Address, B256, U256};

use crate::accounting::{AccountKey, BalanceChange, Effect, State, TokenState};

use super::{
    BlockRef, CandidateTransition, Checkpoint, Finding, Identity, PersistenceError, Status, Store,
};

fn block(number: u64, byte: u8) -> BlockRef {
    BlockRef {
        number,
        hash: B256::repeat_byte(byte),
    }
}

fn identity() -> Identity {
    Identity {
        l1_chain_id: 1,
        zone_chain_id: 2,
        zone_id: 3,
        portal: Address::repeat_byte(4),
        creation: block(5, 5),
    }
}

#[test]
fn rows_survive_restart_and_unwind() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("checker");
    let genesis = block(0, 10);
    let tempo = block(20, 20);
    let token = Address::repeat_byte(30);
    let mut state = State::default();
    state.apply(&[Effect::EnableToken { token }]).unwrap();
    let initial = Store::create_atomic(
        &path,
        &Checkpoint {
            identity: identity(),
            zone: genesis,
            tempo,
            state,
        },
    )
    .unwrap();
    let (store, reopened) = Store::open(&path, identity()).unwrap();
    assert_eq!(reopened, initial);
    assert_eq!(reopened.state.token(token), Some(TokenState::default()));

    let account = AccountKey::new(token, Address::repeat_byte(31));
    let candidate = CandidateTransition::derive(
        reopened,
        block(1, 11),
        genesis,
        block(21, 21),
        &[
            Effect::Credit {
                key: account,
                amount: U256::from(100),
            },
            Effect::PendingTempoRefund {
                token,
                change: BalanceChange::Credit(U256::from(5)),
            },
            Effect::PendingZoneRefund {
                token,
                change: BalanceChange::Credit(U256::from(7)),
            },
        ],
    )
    .unwrap();
    let one = store.apply(candidate).unwrap();
    drop(store);

    let (store, loaded) = Store::open(&path, identity()).unwrap();
    assert_eq!(loaded, one);
    assert_eq!(loaded.state.account(account), Some(U256::from(100)));
    let token_state = loaded.state.token(token).unwrap();
    assert_eq!(token_state.pending_tempo_refunds, U256::from(5));
    assert_eq!(token_state.pending_zone_refunds, U256::from(7));

    let genesis = store.reorg(&loaded, genesis).unwrap();
    assert_eq!(genesis.metadata.verified_zone, block(0, 10));
    assert_eq!(genesis.state.account(account), None);
    assert_eq!(genesis.state.token(token), Some(TokenState::default()));
}

#[test]
fn finding_freezes_verified_tip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("checker");
    let genesis = block(0, 10);
    Store::create_atomic(
        &path,
        &Checkpoint {
            identity: identity(),
            zone: genesis,
            tempo: block(20, 20),
            state: Default::default(),
        },
    )
    .unwrap();
    let (store, snapshot) = Store::open(&path, identity()).unwrap();
    let failed = block(1, 11);
    let diverged = store
        .record_finding(
            &snapshot,
            Finding {
                zone: failed,
                summary: "balance mismatch".into(),
            },
        )
        .unwrap();

    assert_eq!(diverged.metadata.verified_zone, genesis);
    assert_eq!(
        diverged.metadata.status,
        Status::Diverged {
            first_unchecked: failed,
            observed_through: failed,
        }
    );
}

#[test]
fn apply_rejects_stale_candidate_parent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("checker");
    let genesis = block(0, 10);
    Store::create_atomic(
        &path,
        &Checkpoint {
            identity: identity(),
            zone: genesis,
            tempo: block(20, 20),
            state: Default::default(),
        },
    )
    .unwrap();
    let (store, snapshot) = Store::open(&path, identity()).unwrap();
    let stale =
        CandidateTransition::derive(snapshot.clone(), block(1, 11), genesis, block(21, 21), &[])
            .unwrap();
    let current =
        CandidateTransition::derive(snapshot, block(1, 12), genesis, block(21, 21), &[]).unwrap();
    store.apply(current).unwrap();

    assert!(matches!(
        store.apply(stale),
        Err(PersistenceError::StaleSnapshot)
    ));
}
