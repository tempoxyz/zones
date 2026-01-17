//! Prover trait and implementations for generating zone proofs.
//!
//! This module provides:
//! - [`Prover`] trait for proof generation
//! - [`MockProver`] for development and testing
//! - [`Sp1Prover`] placeholder for SP1 SDK integration

use crate::types::{BatchInput, ProofBundle, PublicValues};
use alloy_primitives::Bytes;
use eyre::Result;
use std::future::Future;
use std::pin::Pin;

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
#[derive(Debug, Clone, Default)]
pub struct Sp1ProverConfig {
    /// Path to the ELF binary for the SP1 guest program.
    pub elf_path: String,
    /// Whether to use the prover network or local proving.
    pub use_network: bool,
    /// Network RPC URL for prover network.
    pub network_rpc: Option<String>,
}

/// SP1 prover for generating real ZK proofs.
///
/// This is a placeholder that will be integrated with the SP1 SDK.
#[derive(Debug)]
pub struct Sp1Prover {
    #[allow(dead_code)]
    config: Sp1ProverConfig,
}

impl Sp1Prover {
    /// Creates a new SP1 prover with the given configuration.
    pub const fn new(config: Sp1ProverConfig) -> Self {
        Self { config }
    }
}

impl Prover for Sp1Prover {
    fn prove<'a>(
        &'a self,
        input: &'a BatchInput,
    ) -> Pin<Box<dyn Future<Output = Result<ProofBundle>> + Send + 'a>> {
        Box::pin(async move {
            tracing::warn!(
                blocks = input.blocks.len(),
                "SP1 prover not yet implemented, falling back to mock"
            );

            // TODO: Integrate SP1 SDK
            // 1. Serialize the input for the guest program
            // 2. Create the prover client (network or local)
            // 3. Execute the guest program
            // 4. Generate the proof
            // 5. Return the proof bundle

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
