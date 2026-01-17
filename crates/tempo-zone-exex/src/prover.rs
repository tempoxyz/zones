//! Prover trait and implementations for generating zone proofs.
//!
//! This module provides:
//! - [`Prover`] trait for proof generation
//! - [`MockProver`] for development and testing
//! - [`Sp1Prover`] for real SP1 ZK proof generation (requires `sp1` feature)

use crate::types::{BatchInput, ProofBundle, PublicValues};
use alloy_primitives::Bytes;
use eyre::Result;
use std::future::Future;
use std::pin::Pin;

#[cfg(feature = "sp1")]
use sp1_sdk::{
    EnvProver, HashableKey, Prover as Sp1ProverTrait, ProverClient, SP1ProofMode,
    SP1ProofWithPublicValues, SP1Stdin,
};

/// Trait for generating proofs for zone batches.
pub trait Prover: Send + Sync {
    /// Generates a proof for the given batch input.
    fn prove<'a>(
        &'a self,
        input: &'a BatchInput,
    ) -> Pin<Box<dyn Future<Output = Result<ProofBundle>> + Send + 'a>>;
}

/// Mock prover for development and testing.
///
/// Returns dummy proofs that will be accepted by a mock verifier.
#[derive(Debug, Clone, Default)]
pub struct MockProver;

impl MockProver {
    /// Creates a new mock prover.
    pub const fn new() -> Self {
        Self
    }
}

impl Prover for MockProver {
    fn prove<'a>(
        &'a self,
        input: &'a BatchInput,
    ) -> Pin<Box<dyn Future<Output = Result<ProofBundle>> + Send + 'a>> {
        Box::pin(async move {
            tracing::debug!(
                blocks = input.blocks.len(),
                deposits = input.deposits.len(),
                withdrawals = input.withdrawals.len(),
                "MockProver generating dummy proof"
            );

            let public_values = PublicValues {
                processed_deposit_queue_hash: input.processed_deposit_queue_hash,
                pending_deposit_queue_hash: input.pending_deposit_queue_hash,
                new_processed_deposit_queue_hash: input.new_processed_deposit_queue_hash,
                prev_state_root: input.prev_state_root,
                new_state_root: input.new_state_root,
                expected_withdrawal_queue2: input.expected_withdrawal_queue2,
                updated_withdrawal_queue2: input.updated_withdrawal_queue2,
                new_withdrawal_queue_only: input.new_withdrawal_queue_only,
            };

            Ok(ProofBundle {
                proof: Bytes::from_static(&[0u8; 32]),
                public_values,
                verifier_data: Bytes::new(),
            })
        })
    }
}

/// SP1 prover configuration.
#[derive(Clone)]
pub struct Sp1ProverConfig {
    /// ELF binary for the SP1 guest program.
    pub elf: Vec<u8>,
    /// Whether to use the prover network or local proving.
    /// When true, uses `SP1_PROVER=network` environment variable.
    pub use_network: bool,
}

impl std::fmt::Debug for Sp1ProverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sp1ProverConfig")
            .field("elf_len", &self.elf.len())
            .field("use_network", &self.use_network)
            .finish()
    }
}

/// SP1 prover for generating real ZK proofs.
///
/// Uses the SP1 SDK to generate PLONK proofs suitable for on-chain verification.
/// The prover can run locally or use the SP1 prover network depending on configuration.
#[cfg(feature = "sp1")]
pub struct Sp1Prover {
    client: EnvProver,
    config: Sp1ProverConfig,
}

#[cfg(feature = "sp1")]
impl std::fmt::Debug for Sp1Prover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sp1Prover")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "sp1")]
impl Sp1Prover {
    /// Creates a new SP1 prover with the given configuration.
    ///
    /// The prover client is initialized from environment variables:
    /// - `SP1_PROVER`: "local", "network", or "mock"
    /// - `SP1_PRIVATE_KEY`: Required for network proving
    pub fn new(config: Sp1ProverConfig) -> Self {
        let client = ProverClient::from_env();
        Self { client, config }
    }

    /// Returns the verification key hash for the zone guest program.
    ///
    /// This is used to configure the on-chain verifier contract.
    pub fn vkey_hash(&self) -> String {
        let (_pk, vk) = Sp1ProverTrait::setup(&self.client, &self.config.elf);
        vk.bytes32()
    }
}

#[cfg(feature = "sp1")]
impl Prover for Sp1Prover {
    fn prove<'a>(
        &'a self,
        input: &'a BatchInput,
    ) -> Pin<Box<dyn Future<Output = Result<ProofBundle>> + Send + 'a>> {
        Box::pin(async move {
            tracing::info!(
                blocks = input.blocks.len(),
                deposits = input.deposits.len(),
                withdrawals = input.withdrawals.len(),
                use_network = self.config.use_network,
                "Generating SP1 proof"
            );

            let guest_input = sp1_guest_input_from_batch(input);

            let mut stdin = SP1Stdin::new();
            stdin.write(&guest_input);

            let (pk, vk) = Sp1ProverTrait::setup(&self.client, &self.config.elf);

            let mut proof: SP1ProofWithPublicValues =
                Sp1ProverTrait::prove(&self.client, &pk, &stdin, SP1ProofMode::Plonk)
                    .map_err(|e| eyre::eyre!("SP1 proof generation failed: {e}"))?;

            let public_values = extract_public_values(&mut proof)?;

            let proof_bytes = proof.bytes();

            let vkey_hash: String = vk.bytes32();
            tracing::info!(
                proof_size = proof_bytes.len(),
                vkey = %vkey_hash,
                "SP1 proof generated successfully"
            );

            Ok(ProofBundle {
                proof: Bytes::from(proof_bytes),
                public_values,
                verifier_data: Bytes::from(vkey_hash.as_bytes().to_vec()),
            })
        })
    }
}

#[cfg(feature = "sp1")]
#[allow(unreachable_pub)]
mod sp1_types {
    use alloy_primitives::{Address, Bytes as AlBytes, B256, U128};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Withdrawal {
        pub sender: Address,
        pub to: Address,
        pub amount: U128,
        pub memo: B256,
        pub gas_limit: u64,
        pub fallback_recipient: Address,
        pub data: AlBytes,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Deposit {
        pub l1_block_hash: B256,
        pub l1_block_number: u64,
        pub l1_timestamp: u64,
        pub sender: Address,
        pub to: Address,
        pub amount: U128,
        pub memo: B256,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct StateWitness {
        pub new_state_root: B256,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct GuestBatchInput {
        pub processed_deposit_queue_hash: B256,
        pub pending_deposit_queue_hash: B256,
        pub deposits_consumed: Vec<Deposit>,
        pub prev_state_root: B256,
        pub expected_withdrawal_queue2: B256,
        pub withdrawals: Vec<Withdrawal>,
        pub witness: StateWitness,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct GuestPublicValues {
        pub processed_deposit_queue_hash: B256,
        pub pending_deposit_queue_hash: B256,
        pub new_processed_deposit_queue_hash: B256,
        pub prev_state_root: B256,
        pub new_state_root: B256,
        pub expected_withdrawal_queue2: B256,
        pub updated_withdrawal_queue2: B256,
        pub new_withdrawal_queue_only: B256,
    }
}

#[cfg(feature = "sp1")]
fn sp1_guest_input_from_batch(input: &BatchInput) -> sp1_types::GuestBatchInput {
    sp1_types::GuestBatchInput {
        processed_deposit_queue_hash: input.processed_deposit_queue_hash,
        pending_deposit_queue_hash: input.pending_deposit_queue_hash,
        deposits_consumed: input
            .deposits
            .iter()
            .map(|d| sp1_types::Deposit {
                l1_block_hash: d.l1_block_hash,
                l1_block_number: d.l1_block_number,
                l1_timestamp: d.l1_timestamp,
                sender: d.sender,
                to: d.to,
                amount: d.amount,
                memo: d.memo,
            })
            .collect(),
        prev_state_root: input.prev_state_root,
        expected_withdrawal_queue2: input.expected_withdrawal_queue2,
        withdrawals: input
            .withdrawals
            .iter()
            .map(|w| sp1_types::Withdrawal {
                sender: w.sender,
                to: w.to,
                amount: w.amount,
                memo: w.memo,
                gas_limit: w.gas_limit,
                fallback_recipient: w.fallback_recipient,
                data: w.data.clone(),
            })
            .collect(),
        witness: sp1_types::StateWitness {
            new_state_root: input.new_state_root,
        },
    }
}

#[cfg(feature = "sp1")]
fn extract_public_values(proof: &mut SP1ProofWithPublicValues) -> Result<PublicValues> {
    let pv: sp1_types::GuestPublicValues = proof.public_values.read();

    Ok(PublicValues {
        processed_deposit_queue_hash: pv.processed_deposit_queue_hash,
        pending_deposit_queue_hash: pv.pending_deposit_queue_hash,
        new_processed_deposit_queue_hash: pv.new_processed_deposit_queue_hash,
        prev_state_root: pv.prev_state_root,
        new_state_root: pv.new_state_root,
        expected_withdrawal_queue2: pv.expected_withdrawal_queue2,
        updated_withdrawal_queue2: pv.updated_withdrawal_queue2,
        new_withdrawal_queue_only: pv.new_withdrawal_queue_only,
    })
}

#[cfg(not(feature = "sp1"))]
#[derive(Debug)]
pub struct Sp1Prover {
    #[allow(dead_code)]
    config: Sp1ProverConfig,
}

#[cfg(not(feature = "sp1"))]
impl Sp1Prover {
    pub fn new(config: Sp1ProverConfig) -> Self {
        Self { config }
    }
}

#[cfg(not(feature = "sp1"))]
impl Prover for Sp1Prover {
    fn prove<'a>(
        &'a self,
        _input: &'a BatchInput,
    ) -> Pin<Box<dyn Future<Output = Result<ProofBundle>> + Send + 'a>> {
        Box::pin(async move {
            Err(eyre::eyre!(
                "SP1 prover requires the 'sp1' feature to be enabled"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StateTransitionWitness;
    use alloy_primitives::B256;

    #[tokio::test]
    async fn test_mock_prover() {
        let prover = MockProver::new();
        let input = BatchInput {
            processed_deposit_queue_hash: B256::ZERO,
            pending_deposit_queue_hash: B256::ZERO,
            new_processed_deposit_queue_hash: B256::ZERO,
            prev_state_root: B256::ZERO,
            new_state_root: B256::ZERO,
            expected_withdrawal_queue2: B256::ZERO,
            updated_withdrawal_queue2: B256::ZERO,
            new_withdrawal_queue_only: B256::ZERO,
            blocks: vec![],
            deposits: vec![],
            withdrawals: vec![],
            witness: StateTransitionWitness::Mock,
        };

        let result = prover.prove(&input).await;
        assert!(result.is_ok());
    }
}
