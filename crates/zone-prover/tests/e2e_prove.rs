//! End-to-end integration test for `prove_zone_batch`.
//!
//! Constructs a minimal but complete `BatchWitness` with real MPT proofs,
//! a single zone block, and exercises the full prover pipeline.

use alloy_primitives::{Address, B256, U256};
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
    testutil::{TestAccount, build_zone_state_fixture},
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
/// plus the system transaction sender. No contract code is deployed — system
/// txs will execute as no-op calls (target has no code → immediate success).
fn build_initial_accounts() -> Vec<(Address, TestAccount)> {
    let tempo_block_number: u64 = 100;

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
                    (ZONE_OUTBOX_LAST_BATCH_BASE_SLOT.into(), U256::ZERO), // withdrawalQueueHash
                    (
                        (ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1)).into(),
                        U256::from(1), // withdrawalBatchIndex
                    ),
                ],
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
    // Step 1: Build initial zone state with real MPT proofs.
    let initial_accounts = build_initial_accounts();
    let fixture = build_zone_state_fixture(&initial_accounts);

    println!("Initial state root: {}", fixture.state_root);

    // Step 2: Build the genesis header and compute its hash.
    let genesis_header = build_genesis_header(fixture.state_root);
    let genesis_hash = genesis_header.block_hash();

    println!("Genesis header hash: {genesis_hash}");

    // Step 3: Determine the expected post-execution state.
    //
    // With no contract code at ZoneOutbox, the finalizeWithdrawalBatch system
    // tx is a no-op call. The only state change might be the system tx sender's
    // nonce increment (depends on Tempo EVM config).
    //
    // We start by assuming no state change (expected_state_root == initial).
    // If the prover reports a mismatch, we'll adjust.
    let expected_state_root = fixture.state_root;

    // Step 4: Construct the zone block.
    let zone_block = ZoneBlock {
        number: 1,
        parent_hash: genesis_hash,
        timestamp: 1000,
        beneficiary: SEQUENCER,
        expected_state_root,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: vec![],
    };

    // Step 5: Construct public inputs.
    let public_inputs = PublicInputs {
        prev_block_hash: genesis_hash,
        tempo_block_number: 100,
        anchor_block_number: 100,
        anchor_block_hash: B256::from(U256::from(0xdead)),
        expected_withdrawal_batch_index: 1,
        sequencer: SEQUENCER,
    };

    // Step 6: Assemble the full BatchWitness.
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

    // Step 7: Run the prover.
    let result = prove_zone_batch(witness);

    match &result {
        Ok(output) => {
            println!("Prover succeeded!");
            println!("  prev_block_hash: {}", output.block_transition.prev_block_hash);
            println!("  next_block_hash: {}", output.block_transition.next_block_hash);
            println!(
                "  deposit prev: {}",
                output.deposit_queue_transition.prev_processed_hash
            );
            println!(
                "  deposit next: {}",
                output.deposit_queue_transition.next_processed_hash
            );
            println!(
                "  withdrawal_batch_index: {}",
                output.last_batch.withdrawal_batch_index
            );
        }
        Err(e) => {
            println!("Prover FAILED: {e}");
        }
    }

    // Assert success — if this fails, the error message tells us what to fix.
    let output = result.expect("prove_zone_batch should succeed");

    // Verify output.
    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
    assert_eq!(output.last_batch.withdrawal_batch_index, 1);
}

/// Two-block batch: a non-final block followed by a final block with finalize.
#[test]
fn test_prove_zone_batch_two_blocks() {
    let initial_accounts = build_initial_accounts();
    let fixture = build_zone_state_fixture(&initial_accounts);

    let genesis_header = build_genesis_header(fixture.state_root);
    let genesis_hash = genesis_header.block_hash();

    // Block 1: non-final, no system txs, no user txs.
    let block1 = ZoneBlock {
        number: 1,
        parent_hash: genesis_hash,
        timestamp: 1000,
        beneficiary: SEQUENCER,
        expected_state_root: fixture.state_root, // no state changes
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: None,
        transactions: vec![],
    };

    // Compute block 1's header and hash.
    // We need the transactions_root and receipts_root for an empty block.
    // An empty block has no transactions, so these are both EMPTY_ROOT_HASH.
    let block1_header = ZoneHeader {
        parent_hash: genesis_hash,
        beneficiary: SEQUENCER,
        state_root: fixture.state_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 1,
        timestamp: 1000,
    };
    let block1_hash = block1_header.block_hash();

    // Block 2: final block with finalizeWithdrawalBatch.
    let block2 = ZoneBlock {
        number: 2,
        parent_hash: block1_hash,
        timestamp: 2000,
        beneficiary: SEQUENCER,
        expected_state_root: fixture.state_root, // still no state changes
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

    // Verify we executed 2 blocks: the next_block_hash should be block 2's hash.
    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, block1_hash);
    assert_eq!(output.last_batch.withdrawal_batch_index, 1);
}
