//! SP1 Guest Program for Tempo Zone batch proving.
//!
//! This program runs inside the SP1 zkVM to generate proofs for L2 batches.
//! It validates state transitions and computes commitment hashes for deposits
//! and withdrawals.

#![no_main]
sp1_zkvm::entrypoint!(main);

use crate::types::{BatchInput, PublicValues};

mod deposit_hash;
mod state;
mod types;
mod withdrawal_hash;

pub fn main() {
    // Read batch input from prover
    let input: BatchInput = sp1_zkvm::io::read();

    // Process and validate batch
    let public_values = process_batch(input);

    // Commit public values
    sp1_zkvm::io::commit(&public_values);
}

fn process_batch(input: BatchInput) -> PublicValues {
    // 1. Validate deposits and compute new processed queue hash
    let new_processed_deposit_queue_hash = deposit_hash::compute_new_processed_hash(
        &input.deposits_consumed,
        input.processed_deposit_queue_hash,
    );

    // 2. Validate state transition (or mock in dev mode)
    let new_state_root = state::validate_and_compute_state_root(&input);

    // 3. Compute withdrawal queue hashes
    let (updated_queue2, new_queue_only) = withdrawal_hash::compute_withdrawal_hashes(
        &input.withdrawals,
        input.expected_withdrawal_queue2,
    );

    PublicValues {
        processed_deposit_queue_hash: input.processed_deposit_queue_hash,
        pending_deposit_queue_hash: input.pending_deposit_queue_hash,
        new_processed_deposit_queue_hash,
        prev_state_root: input.prev_state_root,
        new_state_root,
        expected_withdrawal_queue2: input.expected_withdrawal_queue2,
        updated_withdrawal_queue2: updated_queue2,
        new_withdrawal_queue_only: new_queue_only,
    }
}
