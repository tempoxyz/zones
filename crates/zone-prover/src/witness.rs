//! Witness construction for zone batch proving.
//!
//! Builds a [`BatchWitness`] from zone node data by fetching zone blocks,
//! reading state proofs, and constructing the complete witness structure.

use alloy_primitives::{Address, B256};
use eyre::Result;
use zone_primitives::{
    BatchStateProof, BatchWitness, PublicInputs, ZoneBlock, ZoneHeader, ZoneStateWitness,
};

/// Builds a [`BatchWitness`] for proof generation.
///
/// TODO: Fetch full zone state proofs via `eth_getProof`, collect Tempo state
/// proofs, and build the complete witness. Currently uses empty placeholders.
pub struct WitnessBuilder {
    /// Previous block hash (from portal state).
    prev_block_hash: B256,
    /// Previous block header.
    prev_block_header: ZoneHeader,
    /// Zone blocks in this batch.
    zone_blocks: Vec<ZoneBlock>,
    /// Tempo block number for this batch.
    tempo_block_number: u64,
    /// Anchor block number (same as tempo for direct mode).
    anchor_block_number: u64,
    /// Anchor block hash.
    anchor_block_hash: B256,
    /// Expected withdrawal batch index.
    expected_withdrawal_batch_index: u64,
    /// Registered sequencer address.
    sequencer: Address,
}

impl WitnessBuilder {
    /// Create a new witness builder with required public inputs.
    pub fn new(
        prev_block_hash: B256,
        prev_block_header: ZoneHeader,
        sequencer: Address,
        tempo_block_number: u64,
    ) -> Self {
        Self {
            prev_block_hash,
            prev_block_header,
            zone_blocks: Vec::new(),
            tempo_block_number,
            anchor_block_number: tempo_block_number,
            anchor_block_hash: B256::ZERO,
            expected_withdrawal_batch_index: 0,
            sequencer,
        }
    }

    /// Set the anchor block for ancestry mode.
    pub fn with_anchor(mut self, anchor_block_number: u64, anchor_block_hash: B256) -> Self {
        self.anchor_block_number = anchor_block_number;
        self.anchor_block_hash = anchor_block_hash;
        self
    }

    /// Set the expected withdrawal batch index.
    pub fn with_expected_withdrawal_batch_index(mut self, index: u64) -> Self {
        self.expected_withdrawal_batch_index = index;
        self
    }

    /// Add a zone block to the witness.
    pub fn add_zone_block(&mut self, block: ZoneBlock) {
        self.zone_blocks.push(block);
    }

    /// Build the complete [`BatchWitness`].
    pub fn build(self) -> Result<BatchWitness> {
        eyre::ensure!(
            !self.zone_blocks.is_empty(),
            "batch must contain at least one zone block"
        );

        Ok(BatchWitness {
            public_inputs: PublicInputs {
                prev_block_hash: self.prev_block_hash,
                tempo_block_number: self.tempo_block_number,
                anchor_block_number: self.anchor_block_number,
                anchor_block_hash: self.anchor_block_hash,
                expected_withdrawal_batch_index: self.expected_withdrawal_batch_index,
                sequencer: self.sequencer,
            },
            prev_block_header: self.prev_block_header,
            zone_blocks: self.zone_blocks,
            // TODO: populate via eth_getProof
            initial_zone_state: ZoneStateWitness::default(),
            tempo_state_proofs: BatchStateProof::default(),
            tempo_ancestry_headers: Vec::new(),
        })
    }
}
