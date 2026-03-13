//! Batch proof generation service.
//!
//! The [`ProofGenerator`] owns the full proof pipeline: taking per-block
//! witness data from the [`WitnessStore`], generating zone state MPT proofs
//! via the node's [`StateProviderFactory`], fetching L1 proofs over RPC,
//! assembling the [`BatchWitness`], and running the prover.
//!
//! The zone monitor calls [`ProofGenerator::generate_batch_proof`] once per
//! batch instead of performing these steps itself.

use std::time::Duration;

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use eyre::Result;
use reth_provider::StateProviderFactory;
use serde::{Deserialize, Serialize};
use tempo_alloy::TempoNetwork;
use tracing::{debug, info};

#[cfg(feature = "succinct-prover")]
use sp1_sdk::{
    HashableKey, NetworkProver, ProveRequest, Prover, ProverClient, ProvingKey, SP1Stdin,
    network::{FulfillmentStrategy, NetworkMode, signer::NetworkSigner},
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

const NITRO_TEE_REQUEST_VERSION_V1: u8 = 1;
const NITRO_TEE_RESPONSE_VERSION_V1: u8 = 1;
const NITRO_TEE_VERIFIER_CONFIG_V1: u8 = 1;
const NITRO_TEE_PROOF_SYSTEM_SECP256K1: u8 = 2;
const NITRO_TEE_PROOF_V1: u8 = 1;
const NITRO_TEE_SIGNATURE_LEN: usize = 65;
const NITRO_TEE_SIGNING_DOMAIN: &[u8] = b"tempo.zone.nitro-tee.batch.v1";
const ENCODED_BATCH_OUTPUT_LEN: usize = 192;

/// Configuration for the Nitro TEE proof backend.
#[derive(Debug, Clone)]
pub struct NitroTeeProverConfig {
    /// Full URL for the TEE proving endpoint.
    pub endpoint: String,
    /// HTTP timeout applied to the TEE request.
    pub timeout: Duration,
    /// Max response body size accepted from the TEE endpoint.
    pub max_response_bytes: usize,
    /// Optional signer pinning for response signature verification.
    pub expected_signer: Option<Address>,
    /// ZonePortal address this proof is bound to.
    pub portal_address: Address,
}

/// Batch proof generator backed by a Nitro enclave proving service.
///
/// The service receives a full `BatchWitness`, re-executes `prove_zone_batch`,
/// and signs the batch commitment with an enclave-controlled key.
pub struct NitroTeeBatchProofGenerator<Provider> {
    inner: ProofGenerator<Provider>,
    config: NitroTeeProverConfig,
    endpoint: reqwest::Url,
    http_client: reqwest::Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NitroTeeSignContext {
    chain_id: u64,
    portal_address: Address,
    sequencer: Address,
    tempo_block_number: u64,
    anchor_block_number: u64,
    anchor_block_hash: B256,
    expected_withdrawal_batch_index: u64,
    block_from: u64,
    block_to: u64,
    prev_block_hash: B256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NitroTeeProveRequest {
    version: u8,
    context: NitroTeeSignContext,
    witness: zone_prover::types::BatchWitness,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NitroTeeProveResponse {
    version: u8,
    signer: Address,
    signature: Bytes,
    output: zone_prover::types::BatchOutput,
}

fn build_nitro_tee_context(
    from: u64,
    to: u64,
    portal_address: Address,
    witness: &zone_prover::types::BatchWitness,
) -> NitroTeeSignContext {
    NitroTeeSignContext {
        chain_id: witness.chain_id,
        portal_address,
        sequencer: witness.public_inputs.sequencer,
        tempo_block_number: witness.public_inputs.tempo_block_number,
        anchor_block_number: witness.public_inputs.anchor_block_number,
        anchor_block_hash: witness.public_inputs.anchor_block_hash,
        expected_withdrawal_batch_index: witness.public_inputs.expected_withdrawal_batch_index,
        block_from: from,
        block_to: to,
        prev_block_hash: witness.public_inputs.prev_block_hash,
    }
}

fn nitro_tee_message_digest(
    context: &NitroTeeSignContext,
    output: &zone_prover::types::BatchOutput,
) -> B256 {
    let mut preimage = Vec::with_capacity(32 + 8 * 8 + 20 * 2 + 32 * 7);

    preimage.extend_from_slice(keccak256(NITRO_TEE_SIGNING_DOMAIN).as_slice());
    preimage.extend_from_slice(&context.chain_id.to_be_bytes());
    preimage.extend_from_slice(context.portal_address.as_slice());
    preimage.extend_from_slice(context.sequencer.as_slice());
    preimage.extend_from_slice(&context.tempo_block_number.to_be_bytes());
    preimage.extend_from_slice(&context.anchor_block_number.to_be_bytes());
    preimage.extend_from_slice(context.anchor_block_hash.as_slice());
    preimage.extend_from_slice(&context.expected_withdrawal_batch_index.to_be_bytes());
    preimage.extend_from_slice(&context.block_from.to_be_bytes());
    preimage.extend_from_slice(&context.block_to.to_be_bytes());
    preimage.extend_from_slice(context.prev_block_hash.as_slice());

    preimage.extend_from_slice(output.block_transition.prev_block_hash.as_slice());
    preimage.extend_from_slice(output.block_transition.next_block_hash.as_slice());
    preimage.extend_from_slice(
        output
            .deposit_queue_transition
            .prev_processed_hash
            .as_slice(),
    );
    preimage.extend_from_slice(
        output
            .deposit_queue_transition
            .next_processed_hash
            .as_slice(),
    );
    preimage.extend_from_slice(output.withdrawal_queue_hash.as_slice());
    preimage.extend_from_slice(&output.last_batch.withdrawal_batch_index.to_be_bytes());

    keccak256(preimage)
}

fn verify_nitro_tee_output(
    context: &NitroTeeSignContext,
    output: &zone_prover::types::BatchOutput,
) -> Result<()> {
    if output.block_transition.prev_block_hash != context.prev_block_hash {
        return Err(eyre::eyre!(
            "TEE output prev_block_hash mismatch: got {}, expected {}",
            output.block_transition.prev_block_hash,
            context.prev_block_hash
        ));
    }

    if output.last_batch.withdrawal_batch_index != context.expected_withdrawal_batch_index {
        return Err(eyre::eyre!(
            "TEE output withdrawal_batch_index mismatch: got {}, expected {}",
            output.last_batch.withdrawal_batch_index,
            context.expected_withdrawal_batch_index
        ));
    }

    Ok(())
}

fn recover_nitro_tee_signer(message_digest: B256, signature: &[u8]) -> Result<Address> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    if signature.len() != NITRO_TEE_SIGNATURE_LEN {
        return Err(eyre::eyre!(
            "invalid TEE signature length: got {}, expected {}",
            signature.len(),
            NITRO_TEE_SIGNATURE_LEN
        ));
    }

    let sig = Signature::from_slice(&signature[..64])
        .map_err(|e| eyre::eyre!("invalid TEE signature bytes: {e}"))?;
    let mut recovery_id = signature[64];
    if recovery_id >= 27 {
        recovery_id -= 27;
    }
    let recid = RecoveryId::from_byte(recovery_id)
        .ok_or_else(|| eyre::eyre!("invalid TEE recovery id: {recovery_id}"))?;
    let key = VerifyingKey::recover_from_prehash(message_digest.as_slice(), &sig, recid)
        .map_err(|e| eyre::eyre!("failed recovering TEE signer from signature: {e}"))?;
    Ok(Address::from_public_key(&key))
}

fn encode_nitro_tee_verifier_config(signer: Address) -> Bytes {
    // Binary encoding (v1):
    // [0]      version (=1)
    // [1]      proof system (=2 for Nitro TEE secp256k1)
    // [2..22]  signer address
    // [22..54] domain hash (keccak256("tempo.zone.nitro-tee.batch.v1"))
    let mut buf = Vec::with_capacity(54);
    buf.push(NITRO_TEE_VERIFIER_CONFIG_V1);
    buf.push(NITRO_TEE_PROOF_SYSTEM_SECP256K1);
    buf.extend_from_slice(signer.as_slice());
    buf.extend_from_slice(keccak256(NITRO_TEE_SIGNING_DOMAIN).as_slice());
    buf.into()
}

fn encode_nitro_tee_proof(
    output: &zone_prover::types::BatchOutput,
    signature: &[u8],
) -> Result<Bytes> {
    if signature.len() != NITRO_TEE_SIGNATURE_LEN {
        return Err(eyre::eyre!(
            "invalid TEE signature length: got {}, expected {}",
            signature.len(),
            NITRO_TEE_SIGNATURE_LEN
        ));
    }

    // Binary encoding (v1):
    // [0]        proof format version (=1)
    // [1..193]   ABI-packed BatchOutput (192 bytes)
    // [193..258] secp256k1 signature (r||s||v, 65 bytes)
    let output_bytes = encode_batch_output(output);
    if output_bytes.len() != ENCODED_BATCH_OUTPUT_LEN {
        return Err(eyre::eyre!(
            "unexpected encoded batch output length: got {}, expected {}",
            output_bytes.len(),
            ENCODED_BATCH_OUTPUT_LEN
        ));
    }

    let mut buf = Vec::with_capacity(1 + ENCODED_BATCH_OUTPUT_LEN + NITRO_TEE_SIGNATURE_LEN);
    buf.push(NITRO_TEE_PROOF_V1);
    buf.extend_from_slice(&output_bytes);
    buf.extend_from_slice(signature);
    Ok(buf.into())
}

impl<P> NitroTeeBatchProofGenerator<P>
where
    P: StateProviderFactory,
{
    /// Create a new Nitro-TEE-backed proof generator.
    pub fn new(
        provider: P,
        witness_store: SharedWitnessStore,
        l1_provider: DynProvider<TempoNetwork>,
        sequencer: alloy_primitives::Address,
        config: NitroTeeProverConfig,
    ) -> Result<Self> {
        let endpoint = reqwest::Url::parse(&config.endpoint).map_err(|e| {
            eyre::eyre!("invalid Nitro TEE endpoint URL '{}': {e}", config.endpoint)
        })?;
        let http_client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| eyre::eyre!("failed building Nitro TEE HTTP client: {e}"))?;

        Ok(Self {
            inner: ProofGenerator::new(provider, witness_store, l1_provider, sequencer),
            config,
            endpoint,
            http_client,
        })
    }

    async fn prove_batch_witness_via_tee(
        &self,
        from: u64,
        to: u64,
        batch_witness: zone_prover::types::BatchWitness,
    ) -> Result<(Bytes, Bytes)> {
        let context = build_nitro_tee_context(from, to, self.config.portal_address, &batch_witness);
        let request = NitroTeeProveRequest {
            version: NITRO_TEE_REQUEST_VERSION_V1,
            context: context.clone(),
            witness: batch_witness,
        };

        let response = self
            .http_client
            .post(self.endpoint.clone())
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Nitro TEE prove request failed: {e}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| eyre::eyre!("failed reading Nitro TEE response body: {e}"))?;
        if body.len() > self.config.max_response_bytes {
            return Err(eyre::eyre!(
                "Nitro TEE response exceeds limit: got {} bytes, max {}",
                body.len(),
                self.config.max_response_bytes
            ));
        }
        if !status.is_success() {
            let preview_len = body.len().min(256);
            let preview = String::from_utf8_lossy(&body[..preview_len]);
            return Err(eyre::eyre!(
                "Nitro TEE endpoint returned {}: {}",
                status,
                preview
            ));
        }

        let response: NitroTeeProveResponse = serde_json::from_slice(&body)
            .map_err(|e| eyre::eyre!("invalid Nitro TEE response JSON: {e}"))?;
        if response.version != NITRO_TEE_RESPONSE_VERSION_V1 {
            return Err(eyre::eyre!(
                "unsupported Nitro TEE response version: got {}, expected {}",
                response.version,
                NITRO_TEE_RESPONSE_VERSION_V1
            ));
        }
        verify_nitro_tee_output(&context, &response.output)?;

        let digest = nitro_tee_message_digest(&context, &response.output);
        let recovered_signer = recover_nitro_tee_signer(digest, &response.signature)?;
        if recovered_signer != response.signer {
            return Err(eyre::eyre!(
                "TEE signer mismatch: response={}, recovered={}",
                response.signer,
                recovered_signer
            ));
        }
        if let Some(expected) = self.config.expected_signer {
            if expected != recovered_signer {
                return Err(eyre::eyre!(
                    "unexpected TEE signer: got {}, expected {}",
                    recovered_signer,
                    expected
                ));
            }
        }

        let verifier_config = encode_nitro_tee_verifier_config(recovered_signer);
        let proof = encode_nitro_tee_proof(&response.output, &response.signature)?;

        info!(
            from,
            to,
            signer = %recovered_signer,
            proof_len = proof.len(),
            next_block_hash = %response.output.block_transition.next_block_hash,
            withdrawal_queue_hash = %response.output.withdrawal_queue_hash,
            "Nitro TEE proof generated successfully"
        );

        Ok((verifier_config, proof))
    }
}

#[async_trait::async_trait]
impl<P> BatchProofGenerator for NitroTeeBatchProofGenerator<P>
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
        self.prove_batch_witness_via_tee(from, to, batch_witness)
            .await
    }
}

#[cfg(feature = "succinct-prover")]
const SUCCINCT_VERIFIER_CONFIG_V1: u8 = 1;
#[cfg(feature = "succinct-prover")]
const SUCCINCT_PROOF_SYSTEM_SP1_PLONK: u8 = 1;
#[cfg(feature = "succinct-prover")]
const SP1_PUBLIC_VALUES_LEN_BATCH_OUTPUT: usize = 192;
#[cfg(feature = "succinct-prover")]
const SUCCINCT_TEE_RPC_URL: &str = "https://tee.sp1-lumiere.xyz";

/// Configuration for the Succinct (SP1 prover network) proof backend.
///
/// The SDK reads credentials (private key, RPC endpoint) from the environment.
/// See the SP1/Succinct SDK docs for the exact variables expected by
/// `sp1_sdk::NetworkProver::new()`.
///
/// `TeePrivate` routes requests to Succinct's private TEE endpoint and enables
/// TEE attestation (`tee_2fa`) for the proof response.
/// Optional override: `ZONE_PROVER_TEE_RPC_URL`.
#[cfg(feature = "succinct-prover")]
#[derive(Debug, Clone)]
pub struct SuccinctNetworkProverConfig {
    /// Skip local simulation before dispatching the request to the prover
    /// network. Useful when the caller already trusts witness generation.
    pub skip_simulation: bool,
    /// Which Succinct prover network mode to use.
    pub mode: SuccinctNetworkMode,
}

#[cfg(feature = "succinct-prover")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuccinctNetworkMode {
    /// Standard public prover network endpoint.
    Public,
    /// Private TEE endpoint with attestation attached to proof bytes.
    TeePrivate,
}

#[cfg(feature = "succinct-prover")]
impl Default for SuccinctNetworkProverConfig {
    fn default() -> Self {
        Self {
            skip_simulation: false,
            mode: SuccinctNetworkMode::Public,
        }
    }
}

#[cfg(feature = "succinct-prover")]
fn succinct_network_signer_from_env() -> Result<NetworkSigner> {
    let private_key = std::env::var("NETWORK_PRIVATE_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| eyre::eyre!("missing required env: NETWORK_PRIVATE_KEY"))?;

    NetworkSigner::local(&private_key).map_err(|e| eyre::eyre!("invalid NETWORK_PRIVATE_KEY: {e}"))
}

#[cfg(feature = "succinct-prover")]
async fn build_succinct_network_prover(mode: SuccinctNetworkMode) -> Result<NetworkProver> {
    match mode {
        SuccinctNetworkMode::Public => Ok(ProverClient::builder().network().build().await),
        SuccinctNetworkMode::TeePrivate => {
            // In sp1-sdk 6.0.1, `builder().network().private()` uses the TEE RPC URL
            // but leaves `NetworkMode` as `Mainnet`, which routes requests through
            // auction RPC methods and fails with gRPC Unimplemented on the TEE endpoint.
            // We force Reserved mode here so request routing matches the private backend.
            let signer = succinct_network_signer_from_env()?;
            let tee_signers = sp1_sdk::network::tee::get_tee_signers()
                .await
                .map_err(|e| eyre::eyre!("failed to fetch TEE signers: {e}"))?;
            let tee_rpc_url = std::env::var("ZONE_PROVER_TEE_RPC_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| SUCCINCT_TEE_RPC_URL.to_string());

            let prover = NetworkProver::new(signer, &tee_rpc_url, NetworkMode::Reserved)
                .await
                .with_tee_signers(tee_signers);
            Ok(prover)
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

        let prover = build_succinct_network_prover(config.mode).await?;
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
        let mut request = state
            .prover
            .prove(&state.pk, stdin)
            .plonk()
            .skip_simulation(self.config.skip_simulation);
        if self.config.mode == SuccinctNetworkMode::TeePrivate {
            request = request
                // Required when using the private TEE endpoint.
                .strategy(FulfillmentStrategy::Reserved)
                // Attach TEE attestation bytes to proof output.
                .tee_2fa();
        }
        let proof = request
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
            tee_attested = proof.tee_proof.is_some(),
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{U256, keccak256};
    use alloy_provider::ProviderBuilder;
    use axum::{Json, Router, extract::State, routing::post};
    use k256::ecdsa::SigningKey;
    use reth_storage_api::noop::NoopProvider;
    use tokio::{net::TcpListener, task::JoinHandle};
    use zone_prover::{
        execute::{
            self,
            storage::{
                TEMPO_STATE_BLOCK_HASH_SLOT, TEMPO_STATE_PACKED_SLOT, TEMPO_STATE_STATE_ROOT_SLOT,
                ZONE_INBOX_PROCESSED_HASH_SLOT, ZONE_OUTBOX_LAST_BATCH_BASE_SLOT,
            },
        },
        testutil::{TestAccount, build_zone_state_fixture_with_absent, compute_state_root},
        types::*,
    };

    use super::*;

    const TEST_CHAIN_ID: u64 = 13371;
    const TEST_SEQUENCER: Address = Address::ZERO;
    const TEST_PORTAL_ADDRESS: Address = Address::repeat_byte(0x99);

    fn sample_context() -> NitroTeeSignContext {
        NitroTeeSignContext {
            chain_id: TEST_CHAIN_ID,
            portal_address: Address::with_last_byte(0x11),
            sequencer: Address::with_last_byte(0x22),
            tempo_block_number: 10,
            anchor_block_number: 10,
            anchor_block_hash: B256::with_last_byte(0x33),
            expected_withdrawal_batch_index: 7,
            block_from: 100,
            block_to: 120,
            prev_block_hash: B256::with_last_byte(0x44),
        }
    }

    fn sample_output() -> zone_prover::types::BatchOutput {
        zone_prover::types::BatchOutput {
            block_transition: zone_prover::types::BlockTransition {
                prev_block_hash: B256::with_last_byte(0x44),
                next_block_hash: B256::with_last_byte(0x55),
            },
            deposit_queue_transition: zone_prover::types::DepositQueueTransition {
                prev_processed_hash: B256::with_last_byte(0x66),
                next_processed_hash: B256::with_last_byte(0x77),
            },
            withdrawal_queue_hash: B256::with_last_byte(0x88),
            last_batch: zone_prover::types::LastBatchCommitment {
                withdrawal_batch_index: 7,
            },
        }
    }

    #[test]
    fn nitro_tee_signature_recovery_roundtrip() {
        let context = sample_context();
        let output = sample_output();
        let digest = nitro_tee_message_digest(&context, &output);

        let sk = SigningKey::from_slice(&[7u8; 32]).expect("valid test key");
        let (sig, recid) = sk
            .sign_prehash_recoverable(digest.as_slice())
            .expect("signing should succeed");
        let mut sig_bytes = sig.to_bytes().to_vec();
        sig_bytes.push(recid.to_byte() + 27);

        let recovered = recover_nitro_tee_signer(digest, &sig_bytes).expect("recover signer");
        let expected = Address::from_public_key(sk.verifying_key());
        assert_eq!(recovered, expected);
    }

    #[test]
    fn nitro_tee_proof_encoding_layout_is_stable() {
        let output = sample_output();
        let signature = vec![0u8; NITRO_TEE_SIGNATURE_LEN];
        let proof = encode_nitro_tee_proof(&output, &signature).expect("encoding should succeed");
        assert_eq!(
            proof.len(),
            1 + ENCODED_BATCH_OUTPUT_LEN + NITRO_TEE_SIGNATURE_LEN
        );
        assert_eq!(proof[0], NITRO_TEE_PROOF_V1);
    }

    fn pack_tempo_state(block_number: u64) -> U256 {
        U256::from(block_number)
    }

    fn build_initial_accounts(block_numbers: &[u64]) -> Vec<(Address, TestAccount)> {
        let tempo_block_number = 100u64;
        let history_slots: Vec<(U256, U256)> = block_numbers
            .iter()
            .filter(|&&n| n > 0)
            .map(|&n| {
                let slot = U256::from((n - 1) % alloy_eips::eip2935::HISTORY_SERVE_WINDOW as u64);
                (slot, U256::ZERO)
            })
            .collect();
        let history_code = alloy_eips::eip2935::HISTORY_STORAGE_CODE.to_vec();

        vec![
            (
                execute::TEMPO_STATE_ADDRESS,
                TestAccount {
                    nonce: 1,
                    storage: vec![
                        (TEMPO_STATE_BLOCK_HASH_SLOT.into(), U256::from(0xdead)),
                        (TEMPO_STATE_STATE_ROOT_SLOT.into(), U256::from(0xcafe)),
                        (
                            TEMPO_STATE_PACKED_SLOT.into(),
                            pack_tempo_state(tempo_block_number),
                        ),
                    ],
                    ..Default::default()
                },
            ),
            (
                execute::ZONE_INBOX_ADDRESS,
                TestAccount {
                    nonce: 1,
                    storage: vec![(ZONE_INBOX_PROCESSED_HASH_SLOT.into(), U256::ZERO)],
                    ..Default::default()
                },
            ),
            (
                execute::ZONE_OUTBOX_ADDRESS,
                TestAccount {
                    nonce: 1,
                    storage: vec![
                        (ZONE_OUTBOX_LAST_BATCH_BASE_SLOT.into(), U256::ZERO),
                        (
                            (ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1)).into(),
                            U256::from(1),
                        ),
                    ],
                    ..Default::default()
                },
            ),
            (
                alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
                TestAccount {
                    nonce: 1,
                    code_hash: keccak256(&history_code),
                    code: Some(history_code),
                    storage: history_slots,
                    ..Default::default()
                },
            ),
            (Address::ZERO, TestAccount::default()),
        ]
    }

    fn apply_eip2935_writes(
        accounts: &[(Address, TestAccount)],
        block_number: u64,
        parent_hash: B256,
    ) -> Vec<(Address, TestAccount)> {
        let mut result = accounts.to_vec();
        let slot =
            U256::from((block_number - 1) % alloy_eips::eip2935::HISTORY_SERVE_WINDOW as u64);
        let value = U256::from_be_bytes(parent_hash.0);

        if let Some((_, acct)) = result
            .iter_mut()
            .find(|(address, _)| *address == alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS)
        {
            if let Some((_, existing)) = acct.storage.iter_mut().find(|(s, _)| *s == slot) {
                *existing = value;
            } else {
                acct.storage.push((slot, value));
            }
        }

        result
    }

    fn build_genesis_header(state_root: B256) -> ZoneHeader {
        ZoneHeader {
            parent_hash: B256::ZERO,
            beneficiary: TEST_SEQUENCER,
            state_root,
            transactions_root: alloy_trie::EMPTY_ROOT_HASH,
            receipts_root: alloy_trie::EMPTY_ROOT_HASH,
            number: 0,
            timestamp: 0,
        }
    }

    fn build_minimal_batch_witness() -> BatchWitness {
        let absent = [alloy_eips::eip4788::SYSTEM_ADDRESS];
        let initial_accounts = build_initial_accounts(&[1]);
        let fixture = build_zone_state_fixture_with_absent(&initial_accounts, &absent);

        let genesis_header = build_genesis_header(fixture.state_root);
        let genesis_hash = genesis_header.block_hash();
        let post_accounts = apply_eip2935_writes(&initial_accounts, 1, genesis_hash);
        let expected_state_root = compute_state_root(&post_accounts);

        let zone_block = ZoneBlock {
            number: 1,
            parent_hash: genesis_hash,
            timestamp: 1000,
            beneficiary: TEST_SEQUENCER,
            gas_limit: u64::MAX,
            base_fee_per_gas: 0,
            expected_state_root,
            tempo_header_rlp: None,
            deposits: vec![],
            decryptions: vec![],
            finalize_withdrawal_batch_count: Some(U256::ZERO),
            transactions: vec![],
        };

        let public_inputs = PublicInputs {
            prev_block_hash: genesis_hash,
            tempo_block_number: 100,
            anchor_block_number: 100,
            anchor_block_hash: B256::from(U256::from(0xdead)),
            expected_withdrawal_batch_index: 1,
            sequencer: TEST_SEQUENCER,
        };

        BatchWitness {
            public_inputs,
            chain_id: TEST_CHAIN_ID,
            prev_block_header: genesis_header,
            zone_blocks: vec![zone_block],
            initial_zone_state: fixture.witness,
            tempo_state_proofs: BatchStateProof {
                node_pool: alloy_primitives::map::HashMap::default(),
                reads: vec![],
                account_proofs: vec![],
            },
            tempo_ancestry_headers: vec![],
        }
    }

    #[derive(Clone)]
    struct MockNitroTeeState {
        signing_key: SigningKey,
    }

    async fn mock_nitro_tee_handler(
        State(state): State<MockNitroTeeState>,
        Json(request): Json<NitroTeeProveRequest>,
    ) -> Json<NitroTeeProveResponse> {
        assert_eq!(request.version, NITRO_TEE_REQUEST_VERSION_V1);

        let output =
            zone_prover::prove_zone_batch(request.witness).expect("mock TEE prover should succeed");
        let digest = nitro_tee_message_digest(&request.context, &output);
        let (signature, recid) = state
            .signing_key
            .sign_prehash_recoverable(digest.as_slice())
            .expect("mock TEE signer should sign");
        let mut sig_bytes = signature.to_bytes().to_vec();
        sig_bytes.push(recid.to_byte() + 27);

        Json(NitroTeeProveResponse {
            version: NITRO_TEE_RESPONSE_VERSION_V1,
            signer: Address::from_public_key(state.signing_key.verifying_key()),
            signature: sig_bytes.into(),
            output,
        })
    }

    async fn spawn_mock_nitro_tee_server(signing_key: SigningKey) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock Nitro TEE server");
        let addr = listener.local_addr().expect("mock Nitro TEE local addr");
        let app = Router::new()
            .route("/prove-batch", post(mock_nitro_tee_handler))
            .with_state(MockNitroTeeState { signing_key });
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock Nitro TEE server should run");
        });

        (format!("http://{addr}/prove-batch"), handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nitro_tee_batch_proof_roundtrip_via_http() {
        let signing_key = SigningKey::from_slice(&[9u8; 32]).expect("valid test key");
        let expected_signer = Address::from_public_key(signing_key.verifying_key());
        let (endpoint, server_handle) = spawn_mock_nitro_tee_server(signing_key).await;

        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().expect("valid dummy URL"))
            .erased();
        let generator = NitroTeeBatchProofGenerator::new(
            NoopProvider::default(),
            Arc::default(),
            l1_provider,
            TEST_SEQUENCER,
            NitroTeeProverConfig {
                endpoint,
                timeout: Duration::from_secs(5),
                max_response_bytes: 1 << 20,
                expected_signer: Some(expected_signer),
                portal_address: TEST_PORTAL_ADDRESS,
            },
        )
        .expect("Nitro TEE generator should build");

        let batch_witness = build_minimal_batch_witness();
        let expected_output = zone_prover::prove_zone_batch(build_minimal_batch_witness())
            .expect("local prover should succeed");
        let context = build_nitro_tee_context(1, 1, TEST_PORTAL_ADDRESS, &batch_witness);
        let (verifier_config, proof) = generator
            .prove_batch_witness_via_tee(1, 1, batch_witness)
            .await
            .expect("Nitro TEE roundtrip should succeed");

        server_handle.abort();

        assert_eq!(verifier_config.len(), 54);
        assert_eq!(verifier_config[0], NITRO_TEE_VERIFIER_CONFIG_V1);
        assert_eq!(verifier_config[1], NITRO_TEE_PROOF_SYSTEM_SECP256K1);
        assert_eq!(&verifier_config[2..22], expected_signer.as_slice());

        assert_eq!(
            proof.len(),
            1 + ENCODED_BATCH_OUTPUT_LEN + NITRO_TEE_SIGNATURE_LEN
        );
        assert_eq!(proof[0], NITRO_TEE_PROOF_V1);
        assert_eq!(
            &proof[1..1 + ENCODED_BATCH_OUTPUT_LEN],
            encode_batch_output(&expected_output).as_slice()
        );

        let recovered = recover_nitro_tee_signer(
            nitro_tee_message_digest(&context, &expected_output),
            &proof[1 + ENCODED_BATCH_OUTPUT_LEN..],
        )
        .expect("proof signature should recover");
        assert_eq!(recovered, expected_signer);
    }
}
