#![no_main]

sp1_zkvm::entrypoint!(main);

use alloy_primitives::U256;
use zone_stf::{
    prove_zone_batch,
    types::{BatchOutput, BatchWitness},
};

pub fn main() {
    let witness = sp1_zkvm::io::read::<BatchWitness>();
    let output = prove_zone_batch(witness).expect("zone_stf::prove_zone_batch failed");
    let encoded = encode_batch_output(&output);
    sp1_zkvm::io::commit_slice(&encoded);
}

fn encode_batch_output(output: &BatchOutput) -> Vec<u8> {
    let mut buf = Vec::with_capacity(192);
    buf.extend_from_slice(output.block_transition.prev_block_hash.as_slice());
    buf.extend_from_slice(output.block_transition.next_block_hash.as_slice());
    buf.extend_from_slice(output.deposit_queue_transition.prev_processed_hash.as_slice());
    buf.extend_from_slice(output.deposit_queue_transition.next_processed_hash.as_slice());
    buf.extend_from_slice(output.withdrawal_queue_hash.as_slice());
    buf.extend_from_slice(&U256::from(output.last_batch.withdrawal_batch_index).to_be_bytes::<32>());
    buf
}
