//! End-to-end integration test for `prove_zone_batch`.
//!
//! Constructs a minimal but complete `BatchWitness` with real MPT proofs,
//! a single zone block, and exercises the full prover pipeline.

use alloy_primitives::{Address, B256, U256, keccak256};
use zone_prover::{
    prove_zone_batch,
    execute::{
        self,
        storage::{
            TEMPO_STATE_BLOCK_HASH_SLOT, TEMPO_STATE_PACKED_SLOT,
            TEMPO_STATE_STATE_ROOT_SLOT, ZONE_INBOX_PROCESSED_HASH_SLOT,
            ZONE_OUTBOX_LAST_BATCH_BASE_SLOT,
        },
    },
    testutil::{TestAccount, build_zone_state_fixture_with_absent, compute_state_root},
    types::*,
};

/// System tx sender — matches `TEMPO_SYSTEM_TX_SENDER` (Address::ZERO).
const SYSTEM_SENDER: Address = Address::ZERO;

/// Sequencer/beneficiary for test blocks.
const SEQUENCER: Address = Address::ZERO;

/// Zone chain ID used in the test.
const CHAIN_ID: u64 = 13371;

/// Pack a Tempo block number into the TempoState packed slot format.
///
/// The packed slot layout: lowest u64 = tempoBlockNumber.
fn pack_tempo_state(block_number: u64) -> U256 {
    U256::from(block_number)
}

/// Build the initial zone state for a minimal zone.
///
/// Includes all predeploy accounts with their mandatory storage slots,
/// the system transaction sender, and the EIP-2935 history contract.
fn build_initial_accounts(block_numbers: &[u64]) -> Vec<(Address, TestAccount)> {
    let tempo_block_number: u64 = 100;

    // EIP-2935 storage slots needed by the prover's system call (one per block).
    let history_slots: Vec<(U256, U256)> = block_numbers
        .iter()
        .filter(|&&n| n > 0)
        .map(|&n| {
            let slot = U256::from((n - 1) % alloy_eips::eip2935::HISTORY_SERVE_WINDOW as u64);
            (slot, U256::ZERO)
        })
        .collect();

    let history_code = alloy_eips::eip2935::HISTORY_STORAGE_CODE.to_vec();

    vec![
        // TempoState — mandatory slots: blockHash(0), stateRoot(4), packed(7)
        (
            execute::TEMPO_STATE_ADDRESS,
            TestAccount {
                nonce: 1,
                storage: vec![
                    (TEMPO_STATE_BLOCK_HASH_SLOT.into(), U256::from(0xdead)),
                    (TEMPO_STATE_STATE_ROOT_SLOT.into(), U256::from(0xcafe)),
                    (TEMPO_STATE_PACKED_SLOT.into(), pack_tempo_state(tempo_block_number)),
                ],
                ..Default::default()
            },
        ),
        // ZoneInbox — slot 0: processedDepositQueueHash
        (
            execute::ZONE_INBOX_ADDRESS,
            TestAccount {
                nonce: 1,
                storage: vec![
                    (ZONE_INBOX_PROCESSED_HASH_SLOT.into(), U256::ZERO),
                ],
                ..Default::default()
            },
        ),
        // ZoneOutbox — slots 5,6: _lastBatch
        (
            execute::ZONE_OUTBOX_ADDRESS,
            TestAccount {
                nonce: 1,
                storage: vec![
                    (ZONE_OUTBOX_LAST_BATCH_BASE_SLOT.into(), U256::ZERO),
                    (
                        (ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1)).into(),
                        U256::from(1),
                    ),
                ],
                ..Default::default()
            },
        ),
        // EIP-2935 blockhash history contract
        (
            alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
            TestAccount {
                nonce: 1,
                code_hash: keccak256(&history_code),
                code: Some(history_code),
                storage: history_slots,
                ..Default::default()
            },
        ),
        // System tx sender (Address::ZERO) — accessed as tx caller
        (
            SYSTEM_SENDER,
            TestAccount::default(),
        ),
    ]
}

/// Compute post-EIP-2935 accounts: apply the history storage write.
fn apply_eip2935_writes(
    accounts: &[(Address, TestAccount)],
    block_number: u64,
    parent_hash: B256,
) -> Vec<(Address, TestAccount)> {
    let mut result = accounts.to_vec();
    let slot = U256::from((block_number - 1) % alloy_eips::eip2935::HISTORY_SERVE_WINDOW as u64);
    let value = U256::from_be_bytes(parent_hash.0);

    if let Some((_, acct)) = result.iter_mut().find(|(a, _)| {
        *a == alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS
    }) {
        if let Some((_, v)) = acct.storage.iter_mut().find(|(s, _)| *s == slot) {
            *v = value;
        } else {
            acct.storage.push((slot, value));
        }
    }
    result
}

/// Build the previous (genesis) zone block header.
///
/// The prover validates that `keccak(rlp(prev_header)) == public_inputs.prev_block_hash`.
fn build_genesis_header(state_root: B256) -> ZoneHeader {
    ZoneHeader {
        parent_hash: B256::ZERO,
        beneficiary: SEQUENCER,
        state_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 0,
        timestamp: 0,
    }
}

#[test]
fn test_prove_zone_batch_minimal_block() {
    let absent = [alloy_eips::eip4788::SYSTEM_ADDRESS];
    let initial_accounts = build_initial_accounts(&[1]);
    let fixture = build_zone_state_fixture_with_absent(&initial_accounts, &absent);

    let genesis_header = build_genesis_header(fixture.state_root);
    let genesis_hash = genesis_header.block_hash();

    // EIP-2935 writes genesis_hash to the history contract at slot 0.
    let post_accounts = apply_eip2935_writes(&initial_accounts, 1, genesis_hash);
    let expected_state_root = compute_state_root(&post_accounts);

    let zone_block = ZoneBlock {
        number: 1,
        parent_hash: genesis_hash,
        timestamp: 1000,
        beneficiary: SEQUENCER,
        gas_limit: u64::MAX,
        base_fee_per_gas: 0,
        expected_state_root,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: vec![],
    };

    let public_inputs = PublicInputs {
        prev_block_hash: genesis_hash,
        tempo_block_number: 100,
        anchor_block_number: 100,
        anchor_block_hash: B256::from(U256::from(0xdead)),
        expected_withdrawal_batch_index: 1,
        sequencer: SEQUENCER,
    };

    let witness = BatchWitness {
        public_inputs,
        chain_id: CHAIN_ID,
        prev_block_header: genesis_header,
        zone_blocks: vec![zone_block],
        initial_zone_state: fixture.witness,
        tempo_state_proofs: BatchStateProof {
            node_pool: alloy_primitives::map::HashMap::default(),
            reads: vec![],
            account_proofs: vec![],
        },
        tempo_ancestry_headers: vec![],
    };

    let output = prove_zone_batch(witness).expect("prove_zone_batch should succeed");

    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
    assert_eq!(output.last_batch.withdrawal_batch_index, 1);
}

/// Two-block batch: a non-final block followed by a final block with finalize.
#[test]
fn test_prove_zone_batch_two_blocks() {
    let absent = [alloy_eips::eip4788::SYSTEM_ADDRESS];
    let initial_accounts = build_initial_accounts(&[1, 2]);
    let fixture = build_zone_state_fixture_with_absent(&initial_accounts, &absent);

    let genesis_header = build_genesis_header(fixture.state_root);
    let genesis_hash = genesis_header.block_hash();

    // Block 1: EIP-2935 writes genesis_hash to slot 0.
    let post1 = apply_eip2935_writes(&initial_accounts, 1, genesis_hash);
    let block1_state_root = compute_state_root(&post1);

    let block1 = ZoneBlock {
        number: 1,
        parent_hash: genesis_hash,
        timestamp: 1000,
        beneficiary: SEQUENCER,
        gas_limit: u64::MAX,
        base_fee_per_gas: 0,
        expected_state_root: block1_state_root,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: None,
        transactions: vec![],
    };

    let block1_header = ZoneHeader {
        parent_hash: genesis_hash,
        beneficiary: SEQUENCER,
        state_root: block1_state_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 1,
        timestamp: 1000,
    };
    let block1_hash = block1_header.block_hash();

    // Block 2: EIP-2935 writes block1_hash to slot 1 (on top of block 1's state).
    let post2 = apply_eip2935_writes(&post1, 2, block1_hash);
    let block2_state_root = compute_state_root(&post2);

    let block2 = ZoneBlock {
        number: 2,
        parent_hash: block1_hash,
        timestamp: 2000,
        beneficiary: SEQUENCER,
        gas_limit: u64::MAX,
        base_fee_per_gas: 0,
        expected_state_root: block2_state_root,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: vec![],
    };

    let public_inputs = PublicInputs {
        prev_block_hash: genesis_hash,
        tempo_block_number: 100,
        anchor_block_number: 100,
        anchor_block_hash: B256::from(U256::from(0xdead)),
        expected_withdrawal_batch_index: 1,
        sequencer: SEQUENCER,
    };

    let witness = BatchWitness {
        public_inputs,
        chain_id: CHAIN_ID,
        prev_block_header: genesis_header,
        zone_blocks: vec![block1, block2],
        initial_zone_state: fixture.witness,
        tempo_state_proofs: BatchStateProof {
            node_pool: alloy_primitives::map::HashMap::default(),
            reads: vec![],
            account_proofs: vec![],
        },
        tempo_ancestry_headers: vec![],
    };

    let output = prove_zone_batch(witness).expect("two-block batch should succeed");

    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, block1_hash);
    assert_eq!(output.last_batch.withdrawal_batch_index, 1);
}
