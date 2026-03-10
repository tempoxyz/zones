//! SP1 guest program for zone batch proving.
//!
//! This program runs inside the SP1 zkVM and proves correct execution of the
//! zone state transition function. It reads a [`BatchWitness`] from the host,
//! re-executes all zone blocks via revm, and commits the [`BatchOutput`] as
//! public values.

#![no_main]
sp1_zkvm::entrypoint!(main);

use zone_primitives::BatchWitness;
use zone_stf::prove_zone_batch;

pub fn main() {
    let witness: BatchWitness = sp1_zkvm::io::read();

    let output = prove_zone_batch(witness).expect("zone batch STF failed");

    sp1_zkvm::io::commit(&output);
}
