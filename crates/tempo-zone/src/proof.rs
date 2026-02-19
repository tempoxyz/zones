//! Batch proof generation service.
//!
//! The [`ProofGenerator`] owns the full proof pipeline: taking per-block
//! witness data from the [`WitnessStore`], generating zone state MPT proofs
//! via the node's [`StateProviderFactory`], fetching L1 proofs over RPC,
//! assembling the [`BatchWitness`], and running the prover.
//!
//! The zone monitor calls [`ProofGenerator::generate_batch_proof`] once per
//! batch instead of performing these steps itself.

use alloy_primitives::B256;
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use eyre::Result;
use reth_provider::StateProviderFactory;
use tempo_alloy::TempoNetwork;
use tracing::{debug, info, warn};

use crate::witness::{
    AccessSnapshot, FetchedL1Proof, SharedWitnessStore, WitnessGenerator,
    WitnessGeneratorConfig, group_l1_reads_for_proof_fetch, RecordedL1Read,
};

/// Node-internal proof generation service.
///
/// Holds a direct handle to the node's state database (via `StateProviderFactory`)
/// so it can generate zone state MPT proofs without going through RPC. L1 proofs
/// are still fetched over RPC since the L1 chain is external.
pub struct ProofGenerator<Provider> {
    provider: Provider,
    witness_store: SharedWitnessStore,
    witness_generator: WitnessGenerator,
    l1_provider: DynProvider<TempoNetwork>,
}

impl<P> ProofGenerator<P>
where
    P: StateProviderFactory,
{
    /// Create a new proof generator.
    ///
    /// - `provider` — the node's `StateProviderFactory` for direct trie access
    /// - `witness_store` — shared with the builder, which writes per-block data
    /// - `l1_provider` — Tempo L1 RPC provider for `eth_getProof` calls
    /// - `sequencer` — the sequencer address for public inputs
    pub fn new(
        provider: P,
        witness_store: SharedWitnessStore,
        l1_provider: DynProvider<TempoNetwork>,
        sequencer: alloy_primitives::Address,
    ) -> Self {
        let witness_generator =
            WitnessGenerator::new(WitnessGeneratorConfig { sequencer });
        Self {
            provider,
            witness_store,
            witness_generator,
            l1_provider,
        }
    }

    /// Generate a proof for the given zone block range.
    ///
    /// 1. Takes per-block data from the [`WitnessStore`].
    /// 2. Merges all [`AccessSnapshot`]s into a single union.
    /// 3. Generates zone state MPT proofs against S₀ using
    ///    [`StateProviderFactory::state_by_block_hash`] — a direct local trie
    ///    walk, no RPC.
    /// 4. Fetches L1 `eth_getProof` responses for recorded Tempo state reads.
    /// 5. Assembles the [`BatchWitness`] and runs the prover.
    ///
    /// Returns `(verifier_config, proof)` on success, or empty bytes on failure.
    pub async fn generate_batch_proof(
        &self,
        from: u64,
        to: u64,
        tempo_block_number: u64,
        prev_block_hash: B256,
        portal_withdrawal_queue_tail: u64,
    ) -> (alloy_primitives::Bytes, alloy_primitives::Bytes) {
        let empty = || {
            (
                alloy_primitives::Bytes::new(),
                alloy_primitives::Bytes::new(),
            )
        };

        // Take witness data from the store for the block range.
        let block_witnesses = {
            let mut store = self.witness_store.lock().expect("witness store poisoned");
            store.take_range(from, to)
        };

        let expected_count = (to - from + 1) as usize;
        if block_witnesses.len() != expected_count {
            warn!(
                from, to,
                found = block_witnesses.len(),
                expected = expected_count,
                "Missing witness data for some blocks in range, using empty proof"
            );
            return empty();
        }

        let first = &block_witnesses[0].1;
        let chain_id = first.chain_id;
        let prev_block_header = first.prev_block_header.clone();
        let s0_parent_hash = first.parent_block_hash;

        // Merge access snapshots from ALL blocks into a single union.
        let mut merged_accesses = AccessSnapshot::default();
        let mut all_l1_reads = Vec::new();
        let mut zone_blocks = Vec::new();
        let mut tempo_ancestry_headers = Vec::new();

        for (_, bw) in &block_witnesses {
            merged_accesses.merge(&bw.access_snapshot);
            all_l1_reads.extend(bw.l1_reads.iter().cloned());
            zone_blocks.push(bw.zone_block.clone());
            tempo_ancestry_headers.push(bw.tempo_header_rlp.clone());
        }

        info!(
            from, to,
            accounts = merged_accesses.accounts.len(),
            storage_accounts = merged_accesses.storage.len(),
            "Merged access snapshots, generating zone state witness via local trie"
        );

        // Open a state provider for S₀ (parent of the first block in the batch).
        let state_provider = match self.provider.state_by_block_hash(s0_parent_hash) {
            Ok(sp) => sp,
            Err(e) => {
                warn!(
                    error = %e,
                    %s0_parent_hash,
                    "Failed to open state provider for S₀, using empty proof"
                );
                return empty();
            }
        };

        let s0_state_root = prev_block_header.state_root;

        // Generate zone state witness via direct trie walk — no RPC.
        let initial_zone_state = match self
            .witness_generator
            .generate_zone_state_witness(
                &*state_provider,
                s0_state_root,
                &merged_accesses.accounts,
                &merged_accesses.storage,
            ) {
            Ok(w) => w,
            Err(e) => {
                warn!(
                    error = %e,
                    "Failed to generate zone state witness, using empty proof"
                );
                return empty();
            }
        };

        // Fetch L1 eth_getProof responses for recorded reads.
        let l1_proofs = match self.fetch_l1_proofs(&all_l1_reads).await {
            Ok(proofs) => proofs,
            Err(e) => {
                warn!(
                    error = %e,
                    "Failed to fetch L1 proofs, using empty proof"
                );
                return empty();
            }
        };

        // Generate the Tempo state proof from recorded reads + fetched proofs.
        let tempo_state_proofs =
            self.witness_generator
                .generate_tempo_state_proof(&all_l1_reads, &l1_proofs);

        // In direct mode (anchor == tempo), the anchor block hash is the hash
        // of the Tempo header processed by the last advanceTempo in the batch.
        let last_header_rlp = &block_witnesses.last().unwrap().1.tempo_header_rlp;
        let anchor_block_hash = alloy_primitives::keccak256(last_header_rlp);

        let public_inputs = zone_prover::types::PublicInputs {
            prev_block_hash,
            tempo_block_number,
            anchor_block_number: tempo_block_number,
            anchor_block_hash,
            expected_withdrawal_batch_index: portal_withdrawal_queue_tail,
            sequencer: self.witness_generator.sequencer(),
        };

        // Assemble the complete batch witness.
        let batch_witness = self.witness_generator.assemble_witness(
            public_inputs,
            chain_id,
            prev_block_header,
            zone_blocks,
            initial_zone_state,
            tempo_state_proofs,
            tempo_ancestry_headers,
        );

        // Run the prover.
        match zone_prover::prove_zone_batch(batch_witness) {
            Ok(output) => {
                info!(
                    from, to,
                    next_block_hash = %output.block_transition.next_block_hash,
                    withdrawal_queue_hash = %output.withdrawal_queue_hash,
                    "Proof generated successfully"
                );

                let proof_bytes = encode_batch_output(&output);
                (alloy_primitives::Bytes::new(), proof_bytes.into())
            }
            Err(e) => {
                warn!(
                    from, to,
                    error = %e,
                    "Proof generation failed, using empty proof"
                );
                empty()
            }
        }
    }

    /// Fetch `eth_getProof` responses from the Tempo L1 chain for all
    /// recorded L1 reads.
    ///
    /// Groups reads by `(tempo_block_number, account)` and issues one
    /// `eth_getProof` RPC call per group.
    async fn fetch_l1_proofs(
        &self,
        l1_reads: &[RecordedL1Read],
    ) -> Result<Vec<FetchedL1Proof>> {
        use alloy_rpc_types_eth::EIP1186AccountProofResponse;

        if l1_reads.is_empty() {
            return Ok(vec![]);
        }

        let groups = group_l1_reads_for_proof_fetch(l1_reads);
        let mut proofs = Vec::with_capacity(groups.len());

        for ((tempo_block_number, account), slots) in &groups {
            debug!(
                tempo_block_number,
                %account,
                slot_count = slots.len(),
                "Fetching eth_getProof from Tempo L1"
            );

            let storage_keys: Vec<alloy_primitives::StorageKey> =
                slots.iter().copied().map(Into::into).collect();

            let proof_response: EIP1186AccountProofResponse = self
                .l1_provider
                .get_proof(
                    *account,
                    storage_keys,
                )
                .block_id(BlockNumberOrTag::Number(*tempo_block_number).into())
                .await?;

            proofs.push(FetchedL1Proof {
                tempo_block_number: *tempo_block_number,
                proof: proof_response,
            });
        }

        info!(
            proof_count = proofs.len(),
            read_count = l1_reads.len(),
            "Fetched L1 proofs for Tempo state reads"
        );

        Ok(proofs)
    }
}

/// Shared, type-erased proof generator handle.
///
/// The `ProofGenerator` is generic over `Provider`, but the monitor doesn't
/// need to know the concrete provider type. This trait object wrapper allows
/// the monitor to hold a simple `Arc<dyn BatchProofGenerator>`.
#[async_trait::async_trait]
pub trait BatchProofGenerator: Send + Sync {
    async fn generate_batch_proof(
        &self,
        from: u64,
        to: u64,
        tempo_block_number: u64,
        prev_block_hash: B256,
        portal_withdrawal_queue_tail: u64,
    ) -> (alloy_primitives::Bytes, alloy_primitives::Bytes);
}

#[async_trait::async_trait]
impl<P> BatchProofGenerator for ProofGenerator<P>
where
    P: StateProviderFactory + Send + Sync,
{
    async fn generate_batch_proof(
        &self,
        from: u64,
        to: u64,
        tempo_block_number: u64,
        prev_block_hash: B256,
        portal_withdrawal_queue_tail: u64,
    ) -> (alloy_primitives::Bytes, alloy_primitives::Bytes) {
        ProofGenerator::generate_batch_proof(
            self,
            from,
            to,
            tempo_block_number,
            prev_block_hash,
            portal_withdrawal_queue_tail,
        )
        .await
    }
}

/// Encode a [`BatchOutput`] into proof bytes.
///
/// Soft-proof format: ABI-packed concatenation of the output commitment fields.
/// The L1 verifier can decode this to validate the batch transition without a ZK proof.
///
/// Layout (192 bytes):
/// - `[0..32]`   prev_block_hash
/// - `[32..64]`  next_block_hash
/// - `[64..96]`  prev_processed_deposit_hash
/// - `[96..128]` next_processed_deposit_hash
/// - `[128..160]` withdrawal_queue_hash
/// - `[160..192]` withdrawal_batch_index (left-padded u64)
fn encode_batch_output(output: &zone_prover::types::BatchOutput) -> Vec<u8> {
    let mut buf = Vec::with_capacity(192);
    buf.extend_from_slice(output.block_transition.prev_block_hash.as_slice());
    buf.extend_from_slice(output.block_transition.next_block_hash.as_slice());
    buf.extend_from_slice(
        output
            .deposit_queue_transition
            .prev_processed_hash
            .as_slice(),
    );
    buf.extend_from_slice(
        output
            .deposit_queue_transition
            .next_processed_hash
            .as_slice(),
    );
    buf.extend_from_slice(output.withdrawal_queue_hash.as_slice());
    buf.extend_from_slice(
        &alloy_primitives::U256::from(output.last_batch.withdrawal_batch_index)
            .to_be_bytes::<32>(),
    );
    buf
}
