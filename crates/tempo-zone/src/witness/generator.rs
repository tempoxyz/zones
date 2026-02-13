//! Witness generation pipeline.
//!
//! Takes recorded state accesses from [`RecordingDatabase`] and
//! [`RecordingL1StateProvider`] and assembles a complete [`BatchWitness`]
//! that can be passed to [`prove_zone_batch`](zone_prover::prove_zone_batch).

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256, map::HashMap};
use reth_storage_api::StateProvider;
use tracing::{debug, info};
use zone_prover::types::{
    AccountWitness, BatchStateProof, BatchWitness, L1StateRead, PublicInputs,
    ZoneBlock, ZoneHeader, ZoneStateWitness,
};

use super::recording_l1::RecordedL1Read;

/// Configuration for the witness generator.
#[derive(Debug, Clone)]
pub struct WitnessGeneratorConfig {
    /// Sequencer address (for public inputs).
    pub sequencer: Address,
}

/// Generates a [`BatchWitness`] from recorded state accesses.
///
/// This is the bridge between the zone node's normal execution pipeline and
/// the prover. After a batch of zone blocks has been executed with recording
/// enabled, this generates the witness needed for proof generation.
pub struct WitnessGenerator {
    config: WitnessGeneratorConfig,
}

impl WitnessGenerator {
    /// Create a new witness generator.
    pub fn new(config: WitnessGeneratorConfig) -> Self {
        Self { config }
    }

    /// Generate a [`ZoneStateWitness`] from recorded state accesses.
    ///
    /// Reads the current state for all accessed accounts and storage slots,
    /// then generates MPT proofs against the given `state_root`.
    ///
    /// # Arguments
    ///
    /// * `state_provider` - Provider for reading current state and generating proofs
    /// * `state_root` - The zone state root at the start of the batch
    /// * `accessed_accounts` - Set of accounts accessed during execution
    /// * `accessed_storage` - Map of storage slots accessed per account
    pub fn generate_zone_state_witness(
        &self,
        state_provider: &dyn StateProvider,
        state_root: B256,
        accessed_accounts: &BTreeSet<Address>,
        accessed_storage: &BTreeMap<Address, BTreeSet<U256>>,
    ) -> ZoneStateWitness {
        let mut accounts = HashMap::default();

        for &addr in accessed_accounts {
            // Read account data from the state provider.
            let nonce = state_provider
                .account_nonce(&addr)
                .ok()
                .flatten()
                .unwrap_or(0);
            let balance = state_provider
                .account_balance(&addr)
                .ok()
                .flatten()
                .unwrap_or(U256::ZERO);
            let code = state_provider
                .account_code(&addr)
                .ok()
                .flatten();
            let code_hash = code
                .as_ref()
                .map(|c| keccak256(c.bytes_slice()))
                .unwrap_or(B256::ZERO);

            // Read storage slots.
            let mut storage = HashMap::default();
            let mut storage_proofs = HashMap::default();
            if let Some(slots) = accessed_storage.get(&addr) {
                for &slot in slots {
                    let value = state_provider
                        .storage(addr, B256::from(slot))
                        .ok()
                        .flatten()
                        .unwrap_or(U256::ZERO);
                    storage.insert(slot, value);
                    // Storage proofs would be generated from the state trie.
                    // For now, we leave them empty — full proof generation requires
                    // trie access which is not yet wired up.
                    storage_proofs.insert(slot, Vec::new());
                }
            }

            // Account proofs would be generated from the state trie.
            // For now, leave empty as a placeholder.
            let account_proof = Vec::new();

            accounts.insert(
                addr,
                AccountWitness {
                    nonce,
                    balance,
                    code_hash,
                    code: code.map(|c| Bytes::copy_from_slice(c.bytes_slice())),
                    storage,
                    account_proof,
                    storage_proofs,
                },
            );
        }

        debug!(
            accounts = accounts.len(),
            state_root = %state_root,
            "Generated zone state witness"
        );

        ZoneStateWitness {
            accounts,
            state_root,
        }
    }

    /// Generate a [`BatchStateProof`] from recorded L1 reads.
    ///
    /// Deduplicates MPT nodes across all reads to produce the compact
    /// proof structure.
    ///
    /// # Arguments
    ///
    /// * `recorded_reads` - All L1 storage reads captured during batch execution
    pub fn generate_tempo_state_proof(
        &self,
        recorded_reads: &[RecordedL1Read],
    ) -> BatchStateProof {
        // Build the deduplicated node pool and read entries.
        // Full MPT proof generation requires access to the Tempo L1 state trie.
        // For now, we create the structure with empty proofs — the proofs will
        // be populated once we have access to Tempo state trie data (via
        // eth_getProof RPC calls).

        let node_pool = HashMap::default();
        let reads: Vec<L1StateRead> = recorded_reads
            .iter()
            .map(|r| L1StateRead {
                zone_block_index: r.zone_block_index,
                tempo_block_number: r.tempo_block_number,
                account: r.account,
                slot: U256::from_be_bytes(r.slot.0),
                node_path: Vec::new(), // TODO: populate from eth_getProof
                value: U256::from_be_bytes(r.value.0),
            })
            .collect();

        debug!(
            reads = reads.len(),
            "Generated Tempo state proof ({} reads, {} unique nodes)",
            reads.len(),
            node_pool.len(),
        );

        BatchStateProof { node_pool, reads }
    }

    /// Assemble a complete [`BatchWitness`] from all components.
    ///
    /// # Arguments
    ///
    /// * `public_inputs` - Public inputs for the batch
    /// * `prev_block_header` - Previous batch's zone block header
    /// * `zone_blocks` - Zone blocks in this batch
    /// * `zone_state_witness` - Initial zone state with proofs
    /// * `tempo_state_proofs` - Tempo L1 state proofs
    /// * `tempo_ancestry_headers` - Tempo headers for ancestry verification
    pub fn assemble_witness(
        &self,
        public_inputs: PublicInputs,
        prev_block_header: ZoneHeader,
        zone_blocks: Vec<ZoneBlock>,
        zone_state_witness: ZoneStateWitness,
        tempo_state_proofs: BatchStateProof,
        tempo_ancestry_headers: Vec<Vec<u8>>,
    ) -> BatchWitness {
        info!(
            blocks = zone_blocks.len(),
            accounts = zone_state_witness.accounts.len(),
            tempo_reads = tempo_state_proofs.reads.len(),
            "Assembled batch witness"
        );

        BatchWitness {
            public_inputs,
            prev_block_header,
            zone_blocks,
            initial_zone_state: zone_state_witness,
            tempo_state_proofs,
            tempo_ancestry_headers,
        }
    }
}
