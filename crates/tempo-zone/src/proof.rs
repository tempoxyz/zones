//! Batch proof generation service.
//!
//! The [`ProofGenerator`] owns the full proof pipeline: taking per-block
//! witness data from the [`WitnessStore`], generating zone state MPT proofs
//! via the node's [`StateProviderFactory`], fetching L1 proofs over RPC,
//! assembling the [`BatchWitness`], and running the prover.
//!
//! The zone monitor calls [`ProofGenerator::generate_batch_proof`] once per
//! batch instead of performing these steps itself.

use alloy_primitives::{B256, Bytes};
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use eyre::Result;
use reth_provider::StateProviderFactory;
use tempo_alloy::TempoNetwork;
use tracing::{debug, info};

#[cfg(feature = "succinct-prover")]
use sp1_sdk::{
    HashableKey, NetworkProver, ProveRequest, Prover, ProverClient, ProvingKey, SP1Stdin,
};
#[cfg(feature = "succinct-prover")]
use tokio::sync::Mutex as AsyncMutex;
#[cfg(feature = "succinct-prover")]
use zone_prover_sp1_program::ELF as ZONE_PROVER_SP1_ELF;

use crate::witness::{
    AccessSnapshot, FetchedL1Proof, RecordedL1Read, SharedWitnessStore, WitnessGenerator,
    WitnessGeneratorConfig, group_l1_reads_for_proof_fetch,
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
        let witness_generator = WitnessGenerator::new(WitnessGeneratorConfig { sequencer });
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
    /// Returns `(verifier_config, proof)` on success, or an error if witness
    /// data is missing or proof generation fails.
    pub async fn generate_batch_proof(
        &self,
        from: u64,
        to: u64,
        tempo_block_number: u64,
        prev_block_hash: B256,
        expected_withdrawal_batch_index: u64,
    ) -> Result<(Bytes, Bytes)> {
        // TODO(production): Replace soft proof (ABI-packed BatchOutput) with a
        // real ZK proof (SP1) or TEE attestation (SGX/TDX).

        let batch_witness = self
            .build_batch_witness(
                from,
                to,
                tempo_block_number,
                prev_block_hash,
                expected_withdrawal_batch_index,
            )
            .await?;

        // Run the prover.
        let output = zone_prover::prove_zone_batch(batch_witness)
            .map_err(|e| eyre::eyre!("prove_zone_batch failed: {e}"))?;

        info!(
            from,
            to,
            next_block_hash = %output.block_transition.next_block_hash,
            withdrawal_queue_hash = %output.withdrawal_queue_hash,
            "Proof generated successfully"
        );

        let proof_bytes = encode_batch_output(&output);
        Ok((Bytes::new(), proof_bytes.into()))
    }

    /// Assemble a complete [`zone_prover::types::BatchWitness`] from the local
    /// witness store, local zone state trie, and Tempo L1 account/storage proofs.
    async fn build_batch_witness(
        &self,
        from: u64,
        to: u64,
        tempo_block_number: u64,
        prev_block_hash: B256,
        expected_withdrawal_batch_index: u64,
    ) -> Result<zone_prover::types::BatchWitness> {
        // Take witness data from the store for the block range, and prune
        // any stale entries below `from` (e.g., leftover from resyncs).
        let block_witnesses = {
            let mut store = self.witness_store.lock().expect("witness store poisoned");
            let before = store.len();
            store.prune_below(from);
            let pruned = before - store.len();
            if pruned > 0 {
                debug!(
                    pruned,
                    from, "Pruned stale witness entries below batch start"
                );
            }
            store.take_range(from, to).map_err(|missing_block| {
                eyre::eyre!(
                    "missing witness data for block {missing_block} in range [{from}, {to}]"
                )
            })?
        };

        let first = &block_witnesses[0].1;
        let chain_id = first.chain_id;
        let prev_block_header = first.prev_block_header.clone();
        let s0_parent_hash = first.parent_block_hash;

        // Merge access snapshots from ALL blocks into a single union.
        let mut merged_accesses = AccessSnapshot::default();
        let mut all_l1_reads = Vec::new();
        let mut zone_blocks = Vec::new();
        // Ancestry headers are only used in ancestry mode (anchor != tempo).
        // For now we always use direct mode, so this stays empty.
        let tempo_ancestry_headers: Vec<Vec<u8>> = vec![];

        for (_, bw) in &block_witnesses {
            merged_accesses.merge(&bw.access_snapshot);
            all_l1_reads.extend(bw.l1_reads.iter().cloned());
            zone_blocks.push(bw.zone_block.clone());
        }

        info!(
            from,
            to,
            accounts = merged_accesses.accounts.len(),
            storage_accounts = merged_accesses.storage.len(),
            "Merged access snapshots, generating zone state witness via local trie"
        );

        // Open a state provider for S₀ (parent of the first block in the batch).
        let state_provider = self
            .provider
            .state_by_block_hash(s0_parent_hash)
            .map_err(|e| {
                eyre::eyre!("failed to open state provider for S₀ ({s0_parent_hash}): {e}")
            })?;

        let s0_state_root = prev_block_header.state_root;

        // Generate zone state witness via direct trie walk — no RPC.
        let initial_zone_state = self.witness_generator.generate_zone_state_witness(
            &*state_provider,
            s0_state_root,
            &merged_accesses.accounts,
            &merged_accesses.storage,
        )?;

        // Fetch L1 eth_getProof responses for recorded reads.
        let l1_proofs = self.fetch_l1_proofs(&all_l1_reads).await?;

        // Generate the Tempo state proof from recorded reads + fetched proofs.
        let tempo_state_proofs = self
            .witness_generator
            .generate_tempo_state_proof(&all_l1_reads, &l1_proofs)?;

        // In direct mode (anchor == tempo), the anchor block hash is the hash
        // of the Tempo header processed by the last advanceTempo in the batch.
        // If no block in the batch advanced Tempo, the binding carries over from
        // the previous batch and we read the hash from the zone state witness.
        let anchor_block_hash = block_witnesses
            .iter()
            .rev()
            .find_map(|(_, bw)| bw.tempo_header_rlp.as_ref())
            .map(|rlp| alloy_primitives::keccak256(rlp))
            .unwrap_or_else(|| {
                // No advanceTempo in this batch — read from the zone state witness.
                // The initial_zone_state already includes TempoState slot 0 because
                // the WitnessGenerator unconditionally adds it.
                initial_zone_state
                    .accounts
                    .get(&zone_prover::execute::TEMPO_STATE_ADDRESS)
                    .and_then(|acct| {
                        acct.storage
                            .get(&zone_prover::execute::storage::TEMPO_STATE_BLOCK_HASH_SLOT)
                    })
                    .map(|v| B256::from(v.to_be_bytes()))
                    .unwrap_or(B256::ZERO)
            });

        let public_inputs = zone_prover::types::PublicInputs {
            prev_block_hash,
            tempo_block_number,
            anchor_block_number: tempo_block_number,
            anchor_block_hash,
            expected_withdrawal_batch_index,
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
        Ok(batch_witness)
    }

    /// Fetch `eth_getProof` responses from the Tempo L1 chain for all
    /// recorded L1 reads.
    ///
    /// Groups reads by `(tempo_block_number, account)` and issues one
    /// `eth_getProof` RPC call per group.
    async fn fetch_l1_proofs(&self, l1_reads: &[RecordedL1Read]) -> Result<Vec<FetchedL1Proof>> {
        use alloy_rpc_types_eth::EIP1186AccountProofResponse;

        if l1_reads.is_empty() {
            return Ok(vec![]);
        }

        let groups = group_l1_reads_for_proof_fetch(l1_reads);
        let mut proofs = Vec::with_capacity(groups.len());

        // TODO(perf): Fetch L1 proofs concurrently with futures::join_all and a
        // concurrency limiter instead of sequential await.
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
                .get_proof(*account, storage_keys)
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
        expected_withdrawal_batch_index: u64,
    ) -> Result<(Bytes, Bytes)>;
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
        expected_withdrawal_batch_index: u64,
    ) -> Result<(Bytes, Bytes)> {
        ProofGenerator::generate_batch_proof(
            self,
            from,
            to,
            tempo_block_number,
            prev_block_hash,
            expected_withdrawal_batch_index,
        )
        .await
    }
}

#[cfg(feature = "succinct-prover")]
const SUCCINCT_VERIFIER_CONFIG_V1: u8 = 1;
#[cfg(feature = "succinct-prover")]
const SUCCINCT_PROOF_SYSTEM_SP1_PLONK: u8 = 1;
#[cfg(feature = "succinct-prover")]
const SP1_PUBLIC_VALUES_LEN_BATCH_OUTPUT: usize = 192;

/// Configuration for the Succinct (SP1 prover network) proof backend.
///
/// The SDK reads credentials (private key, RPC endpoint) from the environment.
/// See the SP1/Succinct SDK docs for the exact variables expected by
/// `sp1_sdk::NetworkProver::new()`.
#[cfg(feature = "succinct-prover")]
#[derive(Debug, Clone)]
pub struct SuccinctNetworkProverConfig {
    /// Skip local simulation before dispatching the request to the prover
    /// network. Useful when the caller already trusts witness generation.
    pub skip_simulation: bool,
}

#[cfg(feature = "succinct-prover")]
impl Default for SuccinctNetworkProverConfig {
    fn default() -> Self {
        Self {
            skip_simulation: false,
        }
    }
}

#[cfg(feature = "succinct-prover")]
struct SuccinctNetworkProverState {
    prover: NetworkProver,
    pk: sp1_sdk::SP1ProvingKey,
    vk: sp1_sdk::SP1VerifyingKey,
}

/// Batch proof generator backed by Succinct's SP1 prover network.
///
/// This reuses the same witness assembly pipeline as [`ProofGenerator`], but
/// sends the witness to an SP1 guest program that runs `zone_prover::prove_zone_batch`
/// and commits the 192-byte batch-output encoding as public values.
#[cfg(feature = "succinct-prover")]
pub struct SuccinctBatchProofGenerator<Provider> {
    inner: ProofGenerator<Provider>,
    config: SuccinctNetworkProverConfig,
    prover_state: AsyncMutex<SuccinctNetworkProverState>,
}

#[cfg(feature = "succinct-prover")]
impl<P> SuccinctBatchProofGenerator<P>
where
    P: StateProviderFactory,
{
    /// Create a new Succinct-backed proof generator.
    pub async fn new(
        provider: P,
        witness_store: SharedWitnessStore,
        l1_provider: DynProvider<TempoNetwork>,
        sequencer: alloy_primitives::Address,
        config: SuccinctNetworkProverConfig,
    ) -> Result<Self> {
        let inner = ProofGenerator::new(provider, witness_store, l1_provider, sequencer);

        let prover = ProverClient::builder().network().build().await;
        let pk = prover
            .setup(ZONE_PROVER_SP1_ELF)
            .await
            .map_err(|e| eyre::eyre!("SP1 network prover setup failed: {e}"))?;
        let vk = pk.verifying_key().clone();

        Ok(Self {
            inner,
            config,
            prover_state: AsyncMutex::new(SuccinctNetworkProverState { prover, pk, vk }),
        })
    }
}

#[cfg(feature = "succinct-prover")]
#[async_trait::async_trait]
impl<P> BatchProofGenerator for SuccinctBatchProofGenerator<P>
where
    P: StateProviderFactory + Send + Sync,
{
    async fn generate_batch_proof(
        &self,
        from: u64,
        to: u64,
        tempo_block_number: u64,
        prev_block_hash: B256,
        expected_withdrawal_batch_index: u64,
    ) -> Result<(Bytes, Bytes)> {
        let batch_witness = self
            .inner
            .build_batch_witness(
                from,
                to,
                tempo_block_number,
                prev_block_hash,
                expected_withdrawal_batch_index,
            )
            .await?;

        let mut stdin = SP1Stdin::new();
        stdin.write(&batch_witness);

        info!(
            from,
            to, "Dispatching batch proof to Succinct prover network"
        );

        let state = self.prover_state.lock().await;
        let proof = state
            .prover
            .prove(&state.pk, stdin)
            .plonk()
            .skip_simulation(self.config.skip_simulation)
            .await
            .map_err(|e| eyre::eyre!("SP1 network proof failed: {e}"))?;

        let public_values = proof.public_values.to_vec();
        if public_values.len() != SP1_PUBLIC_VALUES_LEN_BATCH_OUTPUT {
            return Err(eyre::eyre!(
                "unexpected SP1 public values length: got {}, expected {}",
                public_values.len(),
                SP1_PUBLIC_VALUES_LEN_BATCH_OUTPUT
            ));
        }

        let vk_hash = B256::from(state.vk.bytes32_raw());
        let verifier_config = encode_succinct_verifier_config(vk_hash, &public_values)?;
        let proof_bytes: Bytes = proof.bytes().into();

        info!(
            from,
            to,
            proof_len = proof_bytes.len(),
            public_values_len = public_values.len(),
            vk_hash = %vk_hash,
            "Succinct proof generated successfully"
        );

        Ok((verifier_config, proof_bytes))
    }
}

#[cfg(feature = "succinct-prover")]
fn encode_succinct_verifier_config(vk_hash: B256, public_values: &[u8]) -> Result<Bytes> {
    let public_values_len = u32::try_from(public_values.len())
        .map_err(|_| eyre::eyre!("SP1 public values too long: {}", public_values.len()))?;

    // Binary encoding (v1):
    // [0]      version (=1)
    // [1]      proof system (=1 for SP1 Plonk)
    // [2..34]  vk hash (bytes32)
    // [34..38] public values len (u32, big-endian)
    // [38..]   public values bytes
    let mut buf = Vec::with_capacity(38 + public_values.len());
    buf.push(SUCCINCT_VERIFIER_CONFIG_V1);
    buf.push(SUCCINCT_PROOF_SYSTEM_SP1_PLONK);
    buf.extend_from_slice(vk_hash.as_slice());
    buf.extend_from_slice(&public_values_len.to_be_bytes());
    buf.extend_from_slice(public_values);
    Ok(buf.into())
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
        &alloy_primitives::U256::from(output.last_batch.withdrawal_batch_index).to_be_bytes::<32>(),
    );
    buf
}
