//! SP1 proving orchestration for zone batches.

use alloy_primitives::Bytes;
use eyre::Result;
use sp1_sdk::{Elf, ProveRequest as _, Prover as _, SP1Stdin};
use tracing::{info, instrument};
use zone_primitives::BatchWitness;

/// The compiled SP1 ELF binary for the zone batch program.
///
/// Once the guest program is compiled to RISC-V ELF via `sp1-build`, replace this
/// with `include_bytes!` or `sp1_sdk::include_elf!`.
const ZONE_BATCH_ELF: &[u8] = &[];

/// Proving mode for the zone batch prover.
#[derive(Debug, Clone, Copy, Default)]
pub enum ProverMode {
    /// Mock mode: validates STF logic without generating a real proof.
    /// Returns empty proof bytes. Useful for development and testing.
    #[default]
    Mock,
    /// CPU mode: generates a real proof using local CPU.
    /// Slow but does not require network access.
    Cpu,
    /// Network mode: submits proving to the Succinct Prover Network.
    /// Fast but requires network access and a valid API key.
    Network,
}

/// Result of a zone batch proof generation.
#[derive(Debug, Clone)]
pub struct ProofResult {
    /// The proof bytes to submit on-chain.
    pub proof: Bytes,
    /// The verifier configuration bytes (e.g., vkey commitment).
    pub verifier_config: Bytes,
}

impl ProofResult {
    /// Create an empty proof result (for POC/mock mode).
    pub fn empty() -> Self {
        Self {
            proof: Bytes::new(),
            verifier_config: Bytes::new(),
        }
    }
}

/// Zone batch prover that orchestrates SP1 proof generation.
pub struct ZoneBatchProver {
    mode: ProverMode,
}

impl ZoneBatchProver {
    /// Create a new prover with the given mode.
    pub fn new(mode: ProverMode) -> Self {
        Self { mode }
    }

    /// Generate a proof for the given batch witness.
    ///
    /// In mock mode, validates the STF logic and returns empty proof bytes.
    /// In CPU/network mode, generates a real SP1 proof.
    #[instrument(skip_all, fields(mode = ?self.mode))]
    pub async fn prove(&self, witness: &BatchWitness) -> Result<ProofResult> {
        match self.mode {
            ProverMode::Mock => self.prove_mock(witness),
            ProverMode::Cpu => self.prove_sp1(witness).await,
            ProverMode::Network => Err(eyre::eyre!("Network proving not yet implemented")),
        }
    }

    /// Mock proof: validate witness structure without generating a real proof.
    fn prove_mock(&self, witness: &BatchWitness) -> Result<ProofResult> {
        info!(
            zone_blocks = witness.zone_blocks.len(),
            "Generating mock proof (no real proof)"
        );

        validate_witness_structure(witness)?;

        info!("Mock proof validation passed");
        Ok(ProofResult::empty())
    }

    /// Generate a real SP1 proof using local CPU.
    async fn prove_sp1(&self, witness: &BatchWitness) -> Result<ProofResult> {
        if ZONE_BATCH_ELF.is_empty() {
            return Err(eyre::eyre!(
                "SP1 guest ELF not available. Compile the zone-batch program first, \
                 or use ProverMode::Mock for testing."
            ));
        }

        let witness = witness.clone();

        // Run proving in a blocking task since SP1 proving is CPU-intensive
        let proof_result = tokio::task::spawn_blocking(move || -> Result<ProofResult> {
            let runtime = tokio::runtime::Handle::current();

            let prover = runtime.block_on(sp1_sdk::ProverClient::builder().cpu().build());
            let elf = Elf::from(ZONE_BATCH_ELF);
            let pk = runtime.block_on(prover.setup(elf))?;

            let mut stdin = SP1Stdin::new();
            stdin.write(&witness);

            info!("Starting SP1 proof generation...");
            let proof = runtime.block_on(async { prover.prove(&pk, stdin).compressed().await })?;

            let proof_bytes = proof.bytes();
            info!(
                proof_size = proof_bytes.len(),
                "SP1 proof generated successfully"
            );

            Ok(ProofResult {
                proof: Bytes::from(proof_bytes),
                verifier_config: Bytes::new(),
            })
        })
        .await??;

        Ok(proof_result)
    }
}

/// Validate the structural correctness of a batch witness.
fn validate_witness_structure(witness: &BatchWitness) -> Result<()> {
    let public_inputs = &witness.public_inputs;

    // Verify previous block header matches public inputs
    let prev_header_hash = witness.prev_block_header.hash();
    eyre::ensure!(
        prev_header_hash == public_inputs.prev_block_hash,
        "prev_block_header hash ({prev_header_hash}) does not match \
         public_inputs.prev_block_hash ({})",
        public_inputs.prev_block_hash,
    );

    // Must have at least one zone block
    eyre::ensure!(
        !witness.zone_blocks.is_empty(),
        "batch must contain at least one zone block"
    );

    // Validate block chain continuity
    let mut prev_hash = public_inputs.prev_block_hash;
    let mut prev_number = witness.prev_block_header.number;

    for (idx, block) in witness.zone_blocks.iter().enumerate() {
        let is_last = idx + 1 == witness.zone_blocks.len();

        eyre::ensure!(
            block.parent_hash == prev_hash,
            "block {} parent_hash mismatch",
            block.number,
        );
        eyre::ensure!(
            block.number == prev_number + 1,
            "block {} number not sequential (expected {})",
            block.number,
            prev_number + 1,
        );
        eyre::ensure!(
            block.beneficiary == public_inputs.sequencer,
            "block {} beneficiary does not match sequencer",
            block.number,
        );

        if is_last {
            eyre::ensure!(
                block.finalize_withdrawal_batch_count.is_some(),
                "final block must have finalize_withdrawal_batch_count",
            );
        } else {
            eyre::ensure!(
                block.finalize_withdrawal_batch_count.is_none(),
                "only the final block may have finalize_withdrawal_batch_count",
            );
        }

        prev_hash = block.parent_hash;
        prev_number = block.number;
    }

    Ok(())
}
