//! Host-side zone batch prover using SP1.
//!
//! Constructs [`BatchWitness`] from zone node data and orchestrates SP1 proving
//! to generate validity proofs for zone state transitions.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod prover;
pub mod witness;

pub use prover::{ProofResult, ProverMode, ZoneBatchProver};
pub use witness::WitnessBuilder;
