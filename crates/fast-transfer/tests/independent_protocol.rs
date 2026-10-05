//! Independent T14 protocol-boundary tests.
//!
//! These exercise public protocol validators only. They do not claim native token-balance
//! coverage; native accounting is covered by the node-runtime acceptance test.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};
use zone_fast_transfer::drain::{BarrierInventory, CheckpointImage, DrainError};
use zone_primitives::fast_transfer::FastCheckpointStatement;

const T14_L1_CHAIN_ID: u64 = 14_014;
const T14_OLD_EPOCH: u64 = 14;
const T14_NEXT_EPOCH: u64 = 15;

fn checkpoint_image() -> CheckpointImage {
    let canonical_head = B256::repeat_byte(0x44);
    let canonical_state = B256::repeat_byte(0x55);
    CheckpointImage {
        l1_chain_id: T14_L1_CHAIN_ID,
        statement: FastCheckpointStatement {
            portal: Address::repeat_byte(0x14),
            old_epoch: T14_OLD_EPOCH,
            next_epoch: T14_NEXT_EPOCH,
            next_roster_hash: B256::repeat_byte(0x15),
            final_zone_height: U256::from(42),
            final_block_hash: canonical_head,
            final_withdrawal_batch_index: 7,
            final_settlement_hash: B256::repeat_byte(0x66),
            checkpoint_log_term: 3,
            checkpoint_log_index: 42,
            checkpoint_height: U256::from(42),
            checkpoint_block_hash: canonical_head,
            checkpoint_state_root: canonical_state,
        },
        canonical_head_hash: canonical_head,
        canonical_state_root: canonical_state,
        witness_root: B256::repeat_byte(0x71),
        outcomes_root: B256::repeat_byte(0x72),
        replay_barriers_root: B256::repeat_byte(0x73),
        raft_prefix: vec![B256::repeat_byte(0x41), canonical_head],
        transfer_history: vec![0x81],
        replay_witnesses: vec![0x82],
        replay_barriers: vec![0x83],
        fast_service_journal: vec![0x84],
        batch_boundaries: vec![0x85],
        replenishment_state: vec![0x86],
        consensus_snapshot: vec![0x87],
    }
}

#[test]
fn t14_checkpoint_rejects_a_prefix_that_does_not_end_at_the_certified_head() {
    let valid = checkpoint_image();
    valid
        .validate()
        .expect("literal T14 checkpoint fixture is valid");

    let mut substituted_prefix = valid;
    *substituted_prefix.raft_prefix.last_mut().unwrap() = B256::repeat_byte(0xee);
    assert_eq!(
        substituted_prefix.validate(),
        Err(DrainError::InvalidCheckpoint),
        "the next roster must install the exact old prefix, not an unrelated nonzero hash list"
    );
}

#[test]
fn t14_destination_barrier_requires_a_canonical_imported_l1_anchor() {
    let result = BarrierInventory::build(
        T14_L1_CHAIN_ID,
        Address::repeat_byte(0x22),
        T14_OLD_EPOCH,
        B256::repeat_byte(0x23),
        Address::repeat_byte(0x11),
        T14_OLD_EPOCH,
        0,
        B256::repeat_byte(0x24),
        2,
        9,
        9,
        B256::repeat_byte(0x25),
        B256::repeat_byte(0x26),
        Vec::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
    );
    assert_eq!(
        result,
        Err(DrainError::InvalidBarrier),
        "a nonzero caller-supplied hash without an imported L1 height is not an authenticated anchor"
    );
}
