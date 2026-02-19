//! Witness generation pipeline.
//!
//! Takes recorded state accesses from [`RecordingDatabase`] and
//! [`RecordingL1StateProvider`] and assembles a complete [`BatchWitness`]
//! that can be passed to [`prove_zone_batch`](zone_prover::prove_zone_batch).
//!
//! ## MPT proof generation
//!
//! **Zone state proofs** are generated using reth's [`StateProofProvider::proof()`],
//! which walks the on-disk Merkle Patricia Trie and returns inclusion proofs for
//! requested accounts and their storage slots. These proofs enable the prover to
//! re-derive account data and storage values from the zone state root alone.
//!
//! **Tempo L1 state proofs** are fetched from the Tempo L1 chain via `eth_getProof`.
//! The caller is responsible for the async RPC calls; this module accepts pre-fetched
//! [`EIP1186AccountProofResponse`]s and deduplicates all MPT nodes into a compact
//! shared pool.
//!
//! [`EIP1186AccountProofResponse`]: alloy_rpc_types_eth::EIP1186AccountProofResponse

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256, map::HashMap};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use reth_storage_api::StateProvider;
use reth_trie_common::TrieInput;
use tracing::{debug, info};
use zone_prover::{
    execute::{
        TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
        storage::{
            TEMPO_STATE_BLOCK_HASH_SLOT, TEMPO_STATE_PACKED_SLOT,
            TEMPO_STATE_STATE_ROOT_SLOT, ZONE_INBOX_PROCESSED_HASH_SLOT,
            ZONE_OUTBOX_LAST_BATCH_BASE_SLOT,
        },
    },
    types::{
        AccountWitness, BatchStateProof, BatchWitness, L1AccountProof, L1StateRead,
        PublicInputs, ZoneBlock, ZoneHeader, ZoneStateWitness,
    },
};

use super::recording_l1::RecordedL1Read;

/// Configuration for the witness generator.
#[derive(Debug, Clone)]
pub struct WitnessGeneratorConfig {
    /// Sequencer address (for public inputs).
    pub sequencer: Address,
}

/// A pre-fetched L1 proof response, tagged with the Tempo block number
/// it was fetched against.
#[derive(Debug, Clone)]
pub struct FetchedL1Proof {
    /// The Tempo block number this proof was retrieved at.
    pub tempo_block_number: u64,
    /// The `eth_getProof` response from the Tempo L1 chain.
    pub proof: EIP1186AccountProofResponse,
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

    /// Returns the sequencer address from the config.
    pub fn sequencer(&self) -> Address {
        self.config.sequencer
    }

    /// Generate a [`ZoneStateWitness`] from recorded state accesses.
    ///
    /// Uses [`StateProofProvider::proof()`] to generate real MPT proofs for each
    /// accessed account and its storage slots against the given `state_root`.
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
    ) -> eyre::Result<ZoneStateWitness> {
        // The prover unconditionally reads predeploy storage slots outside of
        // block execution (TempoState for initial block number and block hash,
        // ZoneInbox for deposit queue hash, ZoneOutbox for last batch). Ensure
        // these accounts and slots are always present in the witness regardless
        // of whether EVM execution touched them.
        let mut accessed_accounts = accessed_accounts.clone();
        let mut accessed_storage = accessed_storage.clone();

        // TempoState: slot 0 (block hash), slot 4 (state root), slot 7 (packed).
        accessed_accounts.insert(TEMPO_STATE_ADDRESS);
        accessed_storage
            .entry(TEMPO_STATE_ADDRESS)
            .or_default()
            .extend([
                TEMPO_STATE_BLOCK_HASH_SLOT,
                TEMPO_STATE_STATE_ROOT_SLOT,
                TEMPO_STATE_PACKED_SLOT,
            ]);

        // ZoneInbox: slot 0 (processed deposit queue hash).
        accessed_accounts.insert(ZONE_INBOX_ADDRESS);
        accessed_storage
            .entry(ZONE_INBOX_ADDRESS)
            .or_default()
            .insert(ZONE_INBOX_PROCESSED_HASH_SLOT);

        // ZoneOutbox: slot 5 (withdrawal queue hash) and slot 6 (batch index).
        accessed_accounts.insert(ZONE_OUTBOX_ADDRESS);
        accessed_storage
            .entry(ZONE_OUTBOX_ADDRESS)
            .or_default()
            .extend([
                ZONE_OUTBOX_LAST_BATCH_BASE_SLOT,
                ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1),
            ]);

        let mut accounts = HashMap::default();
        let mut absent_accounts: HashMap<Address, Vec<Bytes>> = HashMap::default();

        for &addr in &accessed_accounts {
            // Collect storage slots as B256 keys for the proof request.
            let slot_keys: Vec<B256> = accessed_storage
                .get(&addr)
                .map(|slots| {
                    slots.iter().map(|s| B256::from(s.to_be_bytes())).collect()
                })
                .unwrap_or_default();

            // Generate MPT proofs via the state provider's trie.
            // TrieInput::default() means no overlay — proofs are against the
            // committed state that `state_provider` represents.
            let account_proof_result =
                state_provider.proof(TrieInput::default(), addr, &slot_keys);

            let acct_proof = account_proof_result
                .map_err(|e| eyre::eyre!("MPT proof generation failed for {addr}: {e}"))?;

            // If the account doesn't exist in the state trie, store it as an
            // absent account with the exclusion proof so the prover can return
            // Ok(None) from Database::basic() instead of an error.
            if acct_proof.info.is_none() {
                absent_accounts.insert(addr, acct_proof.proof);
                continue;
            }

            let nonce = acct_proof.info.as_ref().map_or(0, |a| a.nonce);
            let balance = acct_proof.info.as_ref().map_or(U256::ZERO, |a| a.balance);
            let code_hash_from_proof = acct_proof
                .info
                .as_ref()
                .and_then(|a| a.bytecode_hash);
            let storage_root = acct_proof.storage_root;
            let proof_nodes = acct_proof.proof;

            // Convert storage proofs: StorageProof -> (U256, Vec<Bytes>)
            let storage_proofs: HashMap<U256, Vec<Bytes>> = acct_proof
                .storage_proofs
                .into_iter()
                .map(|sp| {
                    let slot = U256::from_be_bytes(sp.key.0);
                    (slot, sp.proof)
                })
                .collect();

            // Read code separately (not included in the account proof).
            let code = state_provider.account_code(&addr).ok().flatten();
            // Use keccak256(code) if code is available, otherwise fall back to
            // the bytecode_hash from the account proof. For accounts without
            // code (EOAs), use KECCAK_EMPTY per Ethereum convention.
            let keccak_empty = keccak256([]);
            let code_hash = code
                .as_ref()
                .map(|c| keccak256(c.bytes_slice()))
                .or(code_hash_from_proof)
                .unwrap_or(keccak_empty);

            // Read storage values (the proof gives us proof nodes but we also
            // need the actual slot values in the witness).
            let mut storage = HashMap::default();
            if let Some(slots) = accessed_storage.get(&addr) {
                for &slot in slots {
                    let value = state_provider
                        .storage(addr, B256::from(slot))
                        .ok()
                        .flatten()
                        .unwrap_or(U256::ZERO);
                    storage.insert(slot, value);
                }
            }

            accounts.insert(
                addr,
                AccountWitness {
                    nonce,
                    balance,
                    code_hash,
                    storage_root,
                    code: code.map(|c| Bytes::copy_from_slice(c.bytes_slice())),
                    storage,
                    account_proof: proof_nodes,
                    storage_proofs,
                },
            );
        }

        debug!(
            accounts = accounts.len(),
            absent = absent_accounts.len(),
            state_root = %state_root,
            "Generated zone state witness with MPT proofs"
        );

        Ok(ZoneStateWitness {
            accounts,
            absent_accounts,
            state_root,
        })
    }

    /// Generate a [`BatchStateProof`] from recorded L1 reads and pre-fetched proofs.
    ///
    /// Each `FetchedL1Proof` is an `eth_getProof` response from the Tempo L1 chain.
    /// All MPT nodes from account and storage proofs are deduplicated into a shared
    /// pool, keyed by `keccak256(node_bytes)`. Each read is annotated with a
    /// `storage_path` — the sequence of hashes through the pool that forms the
    /// storage proof path. Account proofs are stored separately in `account_proofs`.
    ///
    /// # Arguments
    ///
    /// * `recorded_reads` - All L1 storage reads captured during batch execution
    /// * `l1_proofs` - Pre-fetched `eth_getProof` results from the Tempo L1 chain
    pub fn generate_tempo_state_proof(
        &self,
        recorded_reads: &[RecordedL1Read],
        l1_proofs: &[FetchedL1Proof],
    ) -> BatchStateProof {
        let mut node_pool: HashMap<B256, Vec<u8>> = HashMap::default();

        // Pre-compute account proof hashes, keyed by (block_number, account).
        let mut account_proof_hashes: BTreeMap<(u64, Address), Vec<B256>> = BTreeMap::new();

        // Pre-compute storage proof hashes, keyed by (block_number, account, slot_b256).
        let mut storage_proof_hashes: BTreeMap<(u64, Address, B256), Vec<B256>> = BTreeMap::new();

        // Build L1AccountProof entries, deduplicated by (tempo_block_number, account).
        let mut account_proofs_map: BTreeMap<(u64, Address), L1AccountProof> = BTreeMap::new();

        for fetched in l1_proofs {
            let block_num = fetched.tempo_block_number;
            let proof = &fetched.proof;

            // Add account proof nodes to the pool and record their hashes.
            let acct_hashes: Vec<B256> = proof
                .account_proof
                .iter()
                .map(|node| {
                    let hash = keccak256(node);
                    node_pool.entry(hash).or_insert_with(|| node.to_vec());
                    hash
                })
                .collect();
            account_proof_hashes.insert((block_num, proof.address), acct_hashes.clone());

            // Build L1AccountProof from the eth_getProof response.
            account_proofs_map
                .entry((block_num, proof.address))
                .or_insert_with(|| L1AccountProof {
                    tempo_block_number: block_num,
                    account: proof.address,
                    nonce: proof.nonce,
                    balance: proof.balance,
                    storage_root: proof.storage_hash,
                    code_hash: proof.code_hash,
                    account_path: acct_hashes,
                });

            // Add storage proof nodes to the pool.
            for sp in &proof.storage_proof {
                let slot_b256 = sp.key.as_b256();
                let sp_hashes: Vec<B256> = sp
                    .proof
                    .iter()
                    .map(|node| {
                        let hash = keccak256(node);
                        node_pool.entry(hash).or_insert_with(|| node.to_vec());
                        hash
                    })
                    .collect();
                storage_proof_hashes.insert((block_num, proof.address, slot_b256), sp_hashes);
            }
        }

        // Build reads with storage_path only (account proof is separate).
        let reads: Vec<L1StateRead> = recorded_reads
            .iter()
            .map(|r| {
                // Only the storage proof hashes for this specific slot.
                let storage_path = storage_proof_hashes
                    .get(&(r.tempo_block_number, r.account, r.slot))
                    .cloned()
                    .unwrap_or_default();

                L1StateRead {
                    zone_block_index: r.zone_block_index,
                    tempo_block_number: r.tempo_block_number,
                    account: r.account,
                    slot: U256::from_be_bytes(r.slot.0),
                    storage_path,
                    value: U256::from_be_bytes(r.value.0),
                }
            })
            .collect();

        let account_proofs: Vec<L1AccountProof> =
            account_proofs_map.into_values().collect();

        info!(
            reads = reads.len(),
            account_proofs = account_proofs.len(),
            unique_nodes = node_pool.len(),
            "Generated Tempo state proof ({} reads, {} account proofs, {} unique nodes)",
            reads.len(),
            account_proofs.len(),
            node_pool.len(),
        );

        BatchStateProof { node_pool, reads, account_proofs }
    }

    /// Assemble a complete [`BatchWitness`] from all components.
    ///
    /// # Arguments
    ///
    /// * `public_inputs` - Public inputs for the batch
    /// * `chain_id` - Zone chain ID for EVM configuration
    /// * `prev_block_header` - Previous batch's zone block header
    /// * `zone_blocks` - Zone blocks in this batch
    /// * `zone_state_witness` - Initial zone state with proofs
    /// * `tempo_state_proofs` - Tempo L1 state proofs
    /// * `tempo_ancestry_headers` - Tempo headers for ancestry verification
    pub fn assemble_witness(
        &self,
        public_inputs: PublicInputs,
        chain_id: u64,
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
            chain_id,
            "Assembled batch witness"
        );

        BatchWitness {
            public_inputs,
            chain_id,
            prev_block_header,
            zone_blocks,
            initial_zone_state: zone_state_witness,
            tempo_state_proofs,
            tempo_ancestry_headers,
        }
    }
}

/// Convenience function: group recorded L1 reads by `(tempo_block_number, account)`
/// and collect all unique slots per group.
///
/// The returned entries can be used to batch `eth_getProof` calls.
pub fn group_l1_reads_for_proof_fetch(
    recorded_reads: &[RecordedL1Read],
) -> BTreeMap<(u64, Address), Vec<B256>> {
    let mut groups: BTreeMap<(u64, Address), BTreeSet<B256>> = BTreeMap::new();
    for r in recorded_reads {
        groups
            .entry((r.tempo_block_number, r.account))
            .or_default()
            .insert(r.slot);
    }
    groups
        .into_iter()
        .map(|(k, slots)| (k, slots.into_iter().collect()))
        .collect()
}
