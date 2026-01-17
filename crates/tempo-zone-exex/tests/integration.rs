//! Integration tests for the tempo-zone-exex end-to-end flow.
//!
//! Tests the full pipeline: commit block → extract withdrawals → batch → mock prove → submit

use alloy_primitives::{keccak256, Address, Bytes, B256, U128};
use alloy_sol_types::{SolCall, SolType, sol_data};
use tempo_zone_exex::{
    BatchBlock, BatchCommitment, BatchConfig, BatchCoordinator, BatchInput, Deposit,
    IZonePortal, MockProver, ProofBundle, Prover, PublicValues, SolBatchCommitment,
    Withdrawal,
};

/// ABI type for withdrawal encoding matching Solidity for hash chain computation.
type WithdrawalHashTuple = (
    sol_data::Address,        // sender
    sol_data::Address,        // to
    sol_data::Uint<128>,      // amount
    sol_data::FixedBytes<32>, // memo
    sol_data::Uint<64>,       // gasLimit
    sol_data::Address,        // fallbackRecipient
    sol_data::Bytes,          // data
    sol_data::FixedBytes<32>, // tailHash
);

/// Compute the hash of a single withdrawal with a tail hash.
fn hash_withdrawal(withdrawal: &Withdrawal, tail_hash: B256) -> B256 {
    let amount: u128 = withdrawal.amount.to();
    let encoded = WithdrawalHashTuple::abi_encode_params(&(
        withdrawal.sender,
        withdrawal.to,
        amount,
        withdrawal.memo,
        withdrawal.gas_limit,
        withdrawal.fallback_recipient,
        withdrawal.data.clone(),
        tail_hash,
    ));
    keccak256(&encoded)
}

/// Compute withdrawal queue hashes for a batch (oldest outermost for O(1) pop).
fn compute_withdrawal_hashes(
    withdrawals: &[Withdrawal],
    expected_queue2: B256,
) -> (B256, B256) {
    let mut updated_queue = expected_queue2;
    let mut new_only_queue = B256::ZERO;

    for withdrawal in withdrawals.iter().rev() {
        new_only_queue = hash_withdrawal(withdrawal, new_only_queue);
        updated_queue = hash_withdrawal(withdrawal, updated_queue);
    }

    (updated_queue, new_only_queue)
}

fn create_test_withdrawal(index: u8) -> Withdrawal {
    Withdrawal {
        sender: Address::repeat_byte(0xAA),
        to: Address::repeat_byte(index),
        amount: U128::from(1_000_000_000_000_000_000u128 * index as u128),
        memo: B256::repeat_byte(index),
        gas_limit: 100_000,
        fallback_recipient: Address::repeat_byte(0xBB),
        data: Bytes::new(),
    }
}

fn create_test_deposit(index: u8) -> Deposit {
    Deposit {
        l1_block_hash: B256::repeat_byte(index),
        l1_block_number: 1000 + index as u64,
        l1_timestamp: 1700000000 + index as u64,
        sender: Address::repeat_byte(0x11),
        to: Address::repeat_byte(index),
        amount: U128::from(1_000_000_000_000_000_000u128),
        memo: B256::ZERO,
    }
}

fn create_test_block(number: u64) -> BatchBlock {
    BatchBlock {
        number,
        hash: B256::repeat_byte(number as u8),
        parent_hash: B256::repeat_byte((number.saturating_sub(1)) as u8),
        state_root: B256::repeat_byte((number + 100) as u8),
        transactions_root: B256::repeat_byte((number + 200) as u8),
        receipts_root: B256::repeat_byte((number + 50) as u8),
    }
}

/// Test that the withdrawal hash chain is correctly computed.
#[test]
fn test_withdrawal_hash_chain_computation() {
    let w1 = create_test_withdrawal(1);
    let w2 = create_test_withdrawal(2);
    let w3 = create_test_withdrawal(3);

    let expected_queue2 = B256::ZERO;
    let (updated_queue, new_only_queue) =
        compute_withdrawal_hashes(&[w1.clone(), w2.clone(), w3.clone()], expected_queue2);

    // Verify the hash chain structure: hash(w1, hash(w2, hash(w3, 0)))
    let inner = hash_withdrawal(&w3, B256::ZERO);
    let middle = hash_withdrawal(&w2, inner);
    let outer = hash_withdrawal(&w1, middle);

    assert_eq!(
        new_only_queue, outer,
        "new_only_queue should match manual computation"
    );
    assert_eq!(
        updated_queue, new_only_queue,
        "with zero expected, updated should equal new_only"
    );

    // With non-zero expected queue, updated should differ from new_only
    let non_zero_expected = B256::repeat_byte(0x42);
    let (updated_with_expected, new_only_with_expected) =
        compute_withdrawal_hashes(&[w1.clone(), w2.clone()], non_zero_expected);

    assert_ne!(
        updated_with_expected, new_only_with_expected,
        "with non-zero expected, queues should differ"
    );
}

/// Test that BatchInput contains correct state roots from blocks.
#[tokio::test]
async fn test_batch_input_state_roots() {
    let mut coordinator = BatchCoordinator::new(BatchConfig {
        batch_interval: std::time::Duration::from_secs(1),
        max_blocks_per_batch: 100,
        outbox_address: Address::ZERO,
    });

    // Initialize with known state
    let initial_state_root = B256::repeat_byte(0x01);
    coordinator.initialize(B256::ZERO, B256::ZERO, initial_state_root, B256::ZERO);

    // Add blocks
    let block1 = create_test_block(1);
    let block2 = create_test_block(2);
    let block2_state_root = block2.state_root;

    coordinator.add_block(block1);
    coordinator.add_block(block2);

    // Flush and verify state roots
    let (_batch_id, batch) = coordinator.flush_batch().expect("should have batch");

    assert_eq!(
        batch.prev_state_root, initial_state_root,
        "prev_state_root should match initial state"
    );
    assert_eq!(
        batch.new_state_root, block2_state_root,
        "new_state_root should match last block's state root"
    );
    assert_eq!(batch.blocks.len(), 2, "should have 2 blocks");
}

/// Test that MockProver generates proofs with correct public values.
#[tokio::test]
async fn test_mock_prover_public_values() {
    let prover = MockProver::new();

    let input = BatchInput {
        processed_deposit_queue_hash: B256::repeat_byte(0x01),
        pending_deposit_queue_hash: B256::repeat_byte(0x02),
        new_processed_deposit_queue_hash: B256::repeat_byte(0x03),
        prev_state_root: B256::repeat_byte(0x04),
        new_state_root: B256::repeat_byte(0x05),
        expected_withdrawal_queue2: B256::repeat_byte(0x06),
        updated_withdrawal_queue2: B256::repeat_byte(0x07),
        new_withdrawal_queue_only: B256::repeat_byte(0x08),
        blocks: vec![create_test_block(1)],
        deposits: vec![create_test_deposit(1)],
        withdrawals: vec![create_test_withdrawal(1)],
        witness: tempo_zone_exex::types::StateTransitionWitness::Mock,
    };

    let proof_bundle = prover.prove(&input).await.expect("proving should succeed");

    // Verify public values match input
    assert_eq!(
        proof_bundle.public_values.processed_deposit_queue_hash,
        input.processed_deposit_queue_hash
    );
    assert_eq!(
        proof_bundle.public_values.pending_deposit_queue_hash,
        input.pending_deposit_queue_hash
    );
    assert_eq!(
        proof_bundle.public_values.new_processed_deposit_queue_hash,
        input.new_processed_deposit_queue_hash
    );
    assert_eq!(
        proof_bundle.public_values.prev_state_root,
        input.prev_state_root
    );
    assert_eq!(
        proof_bundle.public_values.new_state_root,
        input.new_state_root
    );
    assert_eq!(
        proof_bundle.public_values.expected_withdrawal_queue2,
        input.expected_withdrawal_queue2
    );
    assert_eq!(
        proof_bundle.public_values.updated_withdrawal_queue2,
        input.updated_withdrawal_queue2
    );
    assert_eq!(
        proof_bundle.public_values.new_withdrawal_queue_only,
        input.new_withdrawal_queue_only
    );
}

/// Test that submit calldata is correctly encoded for ZonePortal.submitBatch.
#[test]
fn test_submit_calldata_encoding() {
    let commitment = BatchCommitment {
        new_processed_deposit_queue_hash: B256::repeat_byte(0x01),
        new_state_root: B256::repeat_byte(0x02),
    };

    let expected_withdrawal_queue2 = B256::repeat_byte(0x03);
    let updated_withdrawal_queue2 = B256::repeat_byte(0x04);
    let new_withdrawal_queue_only = B256::repeat_byte(0x05);

    let proof_bundle = ProofBundle {
        proof: Bytes::from_static(&[0xAB; 32]),
        public_values: PublicValues {
            processed_deposit_queue_hash: B256::ZERO,
            pending_deposit_queue_hash: B256::ZERO,
            new_processed_deposit_queue_hash: commitment.new_processed_deposit_queue_hash,
            prev_state_root: B256::ZERO,
            new_state_root: commitment.new_state_root,
            expected_withdrawal_queue2,
            updated_withdrawal_queue2,
            new_withdrawal_queue_only,
        },
        verifier_data: Bytes::from_static(&[0xCD; 16]),
    };

    // Encode the calldata
    let sol_commitment: SolBatchCommitment = commitment.clone().into();
    let call = IZonePortal::submitBatchCall {
        commitment: sol_commitment.clone(),
        expectedWithdrawalQueue2: expected_withdrawal_queue2,
        updatedWithdrawalQueue2: updated_withdrawal_queue2,
        newWithdrawalQueueOnly: new_withdrawal_queue_only,
        verifierData: proof_bundle.verifier_data.clone(),
        proof: proof_bundle.proof.clone(),
    };

    let calldata: Bytes = call.abi_encode().into();

    // Verify calldata is non-empty and has correct selector
    assert!(!calldata.is_empty(), "calldata should not be empty");
    assert!(calldata.len() > 4, "calldata should have selector + data");

    // Decode and verify the calldata
    let decoded =
        IZonePortal::submitBatchCall::abi_decode(&calldata).expect("should decode");

    assert_eq!(
        decoded.commitment.newProcessedDepositQueueHash,
        commitment.new_processed_deposit_queue_hash
    );
    assert_eq!(decoded.commitment.newStateRoot, commitment.new_state_root);
    assert_eq!(
        decoded.expectedWithdrawalQueue2,
        expected_withdrawal_queue2
    );
    assert_eq!(decoded.updatedWithdrawalQueue2, updated_withdrawal_queue2);
    assert_eq!(decoded.newWithdrawalQueueOnly, new_withdrawal_queue_only);
    assert_eq!(decoded.verifierData, proof_bundle.verifier_data);
    assert_eq!(decoded.proof, proof_bundle.proof);
}

/// Full end-to-end integration test: commit block → extract withdrawals → batch → prove → prepare submit.
#[tokio::test]
async fn test_end_to_end_flow() {
    // 1. Setup: Initialize batch coordinator with known state
    let mut coordinator = BatchCoordinator::new(BatchConfig {
        batch_interval: std::time::Duration::from_secs(1),
        max_blocks_per_batch: 10,
        outbox_address: Address::repeat_byte(0xFF),
    });

    let initial_state_root = B256::repeat_byte(0x10);
    let initial_deposit_hash = B256::ZERO;
    let initial_withdrawal_queue2 = B256::ZERO;

    coordinator.initialize(
        initial_deposit_hash,
        initial_deposit_hash,
        initial_state_root,
        initial_withdrawal_queue2,
    );

    // 2. Simulate block committed with deposits
    let deposit1 = create_test_deposit(1);
    let deposit2 = create_test_deposit(2);
    coordinator.add_deposit(deposit1.clone());
    coordinator.add_deposit(deposit2.clone());

    // 3. Simulate blocks with withdrawals
    let block1 = create_test_block(1);
    let block2 = create_test_block(2);
    coordinator.add_block(block1);
    coordinator.add_block(block2);

    let withdrawal1 = create_test_withdrawal(1);
    let withdrawal2 = create_test_withdrawal(2);
    coordinator.add_withdrawal(withdrawal1.clone());
    coordinator.add_withdrawal(withdrawal2.clone());

    // 4. Flush batch
    let (_batch_id, mut batch_input) = coordinator.flush_batch().expect("should produce batch");

    assert_eq!(batch_input.blocks.len(), 2);
    assert_eq!(batch_input.deposits.len(), 2);
    assert_eq!(batch_input.withdrawals.len(), 2);
    assert_eq!(batch_input.prev_state_root, initial_state_root);

    // 5. Compute withdrawal hashes (normally done by prover)
    let (updated_queue2, new_queue_only) = compute_withdrawal_hashes(
        &batch_input.withdrawals,
        batch_input.expected_withdrawal_queue2,
    );
    batch_input.updated_withdrawal_queue2 = updated_queue2;
    batch_input.new_withdrawal_queue_only = new_queue_only;

    // 6. Generate proof with MockProver
    let prover = MockProver::new();
    let proof_bundle = prover
        .prove(&batch_input)
        .await
        .expect("proving should succeed");

    // Verify proof public values
    assert_eq!(
        proof_bundle.public_values.prev_state_root,
        batch_input.prev_state_root
    );
    assert_eq!(
        proof_bundle.public_values.new_state_root,
        batch_input.new_state_root
    );
    assert_eq!(
        proof_bundle.public_values.updated_withdrawal_queue2,
        updated_queue2
    );
    assert_eq!(
        proof_bundle.public_values.new_withdrawal_queue_only,
        new_queue_only
    );

    // 7. Prepare submit calldata (mock L1 submission)
    let commitment = BatchCommitment {
        new_processed_deposit_queue_hash: batch_input.new_processed_deposit_queue_hash,
        new_state_root: batch_input.new_state_root,
    };

    let sol_commitment: SolBatchCommitment = commitment.clone().into();
    let call = IZonePortal::submitBatchCall {
        commitment: sol_commitment,
        expectedWithdrawalQueue2: batch_input.expected_withdrawal_queue2,
        updatedWithdrawalQueue2: batch_input.updated_withdrawal_queue2,
        newWithdrawalQueueOnly: batch_input.new_withdrawal_queue_only,
        verifierData: proof_bundle.verifier_data.clone(),
        proof: proof_bundle.proof.clone(),
    };

    let calldata: Bytes = call.abi_encode().into();

    // Verify calldata can be decoded correctly
    let decoded =
        IZonePortal::submitBatchCall::abi_decode(&calldata).expect("should decode calldata");

    assert_eq!(
        decoded.commitment.newProcessedDepositQueueHash,
        batch_input.new_processed_deposit_queue_hash
    );
    assert_eq!(decoded.commitment.newStateRoot, batch_input.new_state_root);
    assert_eq!(
        decoded.expectedWithdrawalQueue2,
        batch_input.expected_withdrawal_queue2
    );
    assert_eq!(
        decoded.updatedWithdrawalQueue2,
        batch_input.updated_withdrawal_queue2
    );
    assert_eq!(
        decoded.newWithdrawalQueueOnly,
        batch_input.new_withdrawal_queue_only
    );

    // Verify withdrawal hash chain correctness
    assert_ne!(
        batch_input.updated_withdrawal_queue2,
        B256::ZERO,
        "updated_withdrawal_queue2 should not be zero with withdrawals"
    );
    assert_ne!(
        batch_input.new_withdrawal_queue_only,
        B256::ZERO,
        "new_withdrawal_queue_only should not be zero with withdrawals"
    );
    // With zero initial queue, both should be equal
    assert_eq!(
        batch_input.updated_withdrawal_queue2,
        batch_input.new_withdrawal_queue_only,
        "with zero initial queue, updated should equal new_only"
    );
}

/// Test deposit hash chain computation through the batcher.
#[tokio::test]
async fn test_deposit_hash_chain_through_batcher() {
    let mut coordinator = BatchCoordinator::new(BatchConfig::default());
    coordinator.initialize(B256::ZERO, B256::ZERO, B256::ZERO, B256::ZERO);

    // Add deposits
    let deposit1 = create_test_deposit(1);
    let deposit2 = create_test_deposit(2);

    coordinator.add_deposit(deposit1.clone());
    let hash_after_first = coordinator.deposit_tracker().pending_deposit_queue_hash();
    assert_ne!(hash_after_first, B256::ZERO);

    coordinator.add_deposit(deposit2.clone());
    let hash_after_second = coordinator.deposit_tracker().pending_deposit_queue_hash();
    assert_ne!(hash_after_second, hash_after_first);

    // Verify the hash chain is deterministic
    let expected_first = tempo_zone_exex::compute_deposit_hash(&deposit1, B256::ZERO);
    assert_eq!(hash_after_first, expected_first);

    let expected_second = tempo_zone_exex::compute_deposit_hash(&deposit2, expected_first);
    assert_eq!(hash_after_second, expected_second);
}

/// Test batch flushing updates coordinator state correctly.
#[tokio::test]
async fn test_batch_state_updates() {
    let mut coordinator = BatchCoordinator::new(BatchConfig::default());

    let initial_state = B256::repeat_byte(0x01);
    coordinator.initialize(B256::ZERO, B256::ZERO, initial_state, B256::ZERO);

    // Add a block with new state root
    let block = create_test_block(1);
    let block_state_root = block.state_root;
    coordinator.add_block(block);

    // First batch
    let (_batch_id, batch1) = coordinator.flush_batch().expect("should have batch");
    assert_eq!(batch1.prev_state_root, initial_state);
    assert_eq!(batch1.new_state_root, block_state_root);

    // After flush, coordinator should be empty
    assert!(coordinator.is_empty());
    assert!(coordinator.flush_batch().is_none());

    // Simulate batch submission callback
    coordinator.on_batch_submitted(
        batch1.new_processed_deposit_queue_hash,
        batch1.deposits.len(),
        batch1.new_state_root,
        B256::repeat_byte(0x99), // updated withdrawal queue
    );

    // Add another block
    let block2 = create_test_block(2);
    let block2_state_root = block2.state_root;
    coordinator.add_block(block2);

    // Second batch should use updated state
    let (_batch_id, batch2) = coordinator.flush_batch().expect("should have batch");
    assert_eq!(
        batch2.prev_state_root, block_state_root,
        "prev_state_root should be updated after submission"
    );
    assert_eq!(batch2.new_state_root, block2_state_root);
    assert_eq!(
        batch2.expected_withdrawal_queue2,
        B256::repeat_byte(0x99),
        "expected_withdrawal_queue2 should be updated after submission"
    );
}

/// Test that empty batches are not produced.
#[test]
fn test_empty_batch_not_produced() {
    let mut coordinator = BatchCoordinator::new(BatchConfig::default());
    coordinator.initialize(B256::ZERO, B256::ZERO, B256::ZERO, B256::ZERO);

    // Should not produce batch without blocks
    assert!(coordinator.flush_batch().is_none());
    assert!(!coordinator.should_flush());

    // Add deposit without block - still no batch
    coordinator.add_deposit(create_test_deposit(1));
    assert!(coordinator.flush_batch().is_none());
}

/// Test withdrawal extraction from multiple sources.
#[tokio::test]
async fn test_multiple_withdrawal_sources() {
    let mut coordinator = BatchCoordinator::new(BatchConfig::default());
    coordinator.initialize(B256::ZERO, B256::ZERO, B256::ZERO, B256::ZERO);

    coordinator.add_block(create_test_block(1));

    // Add withdrawals directly
    coordinator.add_withdrawal(create_test_withdrawal(1));
    coordinator.add_withdrawal(create_test_withdrawal(2));
    coordinator.add_withdrawal(create_test_withdrawal(3));

    let (_batch_id, batch) = coordinator.flush_batch().expect("should have batch");
    assert_eq!(batch.withdrawals.len(), 3);

    // Verify withdrawal ordering is preserved
    assert_eq!(batch.withdrawals[0].to, Address::repeat_byte(1));
    assert_eq!(batch.withdrawals[1].to, Address::repeat_byte(2));
    assert_eq!(batch.withdrawals[2].to, Address::repeat_byte(3));
}
