//! Witness generation for the zone prover.
//!
//! This module provides the components needed to record state accesses during
//! zone block execution and generate the complete [`BatchWitness`] for the
//! zone prover.
//!
//! ## Components
//!
//! - [`RecordingDatabase`] — wraps any revm `Database` to log all account and
//!   storage accesses during EVM execution.
//! - [`RecordingL1StateProvider`] — wraps the L1 state provider to capture all
//!   Tempo L1 storage reads during `TempoStateReader` precompile calls.
//! - [`WitnessGenerator`] — assembles the complete [`BatchWitness`] from recorded
//!   accesses and state provider data.
//!
//! ## Flow
//!
//! 1. Wrap the zone database with [`RecordingDatabase`] before block execution.
//! 2. Wrap the L1 state provider with [`RecordingL1StateProvider`].
//! 3. Execute the zone block(s) normally.
//! 4. Extract recorded accesses from both wrappers.
//! 5. Use [`WitnessGenerator`] to produce the [`BatchWitness`].

pub mod generator;
pub mod recording_db;
pub mod recording_l1;
pub mod store;

pub use generator::{
    FetchedL1Proof, WitnessGenerator, WitnessGeneratorConfig, group_l1_reads_for_proof_fetch,
};
pub use recording_db::{RecordedAccesses, RecordingDatabase};
pub use recording_l1::{RecordedL1Read, RecordingL1StateProvider, SharedRecordedReads};
pub use store::{BuiltBlockWitness, SharedWitnessStore, WitnessStore};
