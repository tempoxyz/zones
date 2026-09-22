//! Local use of the native verifier's shared logic, independent of L1 fork activation.

use std::str::FromStr;

use alloy_primitives::{FixedBytes, U256};
use tempo_shadow_precompiles::{
    error::TempoPrecompileError,
    storage::{PrecompileStorageProvider, StorageCtx, hashmap::HashMapStorageProvider},
    zone_factory::portal_address,
    zone_verifier::{IZoneVerifier, ZoneVerifier},
};
use zone_spf::{BatchOutput, PublicInputs};

use crate::ProofBundle;

/// Independently pinned PCR0, PCR1 and PCR2 for observational Nitro verification.
///
/// These measurements must come from the approved EIF build, never from a prover response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowProofVerifier {
    pcrs: [[u8; 48]; 3],
}

#[derive(Debug, thiserror::Error)]
pub enum ShadowProofVerificationError {
    #[error("expected three comma-separated 48-byte PCR measurements (PCR0,PCR1,PCR2)")]
    InvalidMeasurements,
    #[error("zero/debug enclave PCR measurements are not permitted")]
    DebugMeasurements,
    #[error("Nitro verification failed: {0}")]
    Verification(#[from] TempoPrecompileError),
}

impl FromStr for ShadowProofVerifier {
    type Err = ShadowProofVerificationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let values = value
            .split(',')
            .map(|part| part.trim().parse::<FixedBytes<48>>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ShadowProofVerificationError::InvalidMeasurements)?;
        let values: [FixedBytes<48>; 3] = values
            .try_into()
            .map_err(|_| ShadowProofVerificationError::InvalidMeasurements)?;
        if values.iter().any(FixedBytes::is_zero) {
            return Err(ShadowProofVerificationError::DebugMeasurements);
        }
        Ok(Self {
            pcrs: values.map(|value| value.0),
        })
    }
}

impl ShadowProofVerifier {
    /// Verify the complete attestation and its batch commitments using the native verifier's
    /// implementation. `timestamp` is the current parent-chain block timestamp, fetched after
    /// proving (not the historical batch timestamp or a prover-supplied timestamp).
    pub fn verify(
        &self,
        inputs: &PublicInputs,
        output: &BatchOutput,
        bundle: &ProofBundle,
        timestamp: u64,
    ) -> Result<bool, ShadowProofVerificationError> {
        let call = verifier_call(inputs, output, bundle);
        let mut storage = HashMapStorageProvider::new(inputs.parent_chain_id);
        storage.set_timestamp(U256::from(timestamp));
        // Reserve all but the observer's budget from the provider's initial gas allowance.
        storage.deduct_gas(storage.gas_limit() - 30_000_000)?;
        StorageCtx::enter(&mut storage, || {
            ZoneVerifier::new().verify_with_pcrs(portal_address(inputs.zone_id), call, self.pcrs)
        })
        .map_err(Into::into)
    }
}

fn verifier_call(
    inputs: &PublicInputs,
    output: &BatchOutput,
    bundle: &ProofBundle,
) -> IZoneVerifier::verifyCall {
    IZoneVerifier::verifyCall {
        zoneId: inputs.zone_id,
        tempoBlockNumber: inputs.tempo_block_number,
        anchorBlockNumber: inputs.anchor_block_number,
        anchorBlockHash: inputs.anchor_block_hash,
        expectedWithdrawalBatchIndex: inputs.expected_withdrawal_batch_index,
        nextZoneHeight: U256::from(output.next_zone_height),
        blockTransition: IZoneVerifier::BlockTransition {
            prevBlockHash: output.block_transition.prevBlockHash,
            nextBlockHash: output.block_transition.nextBlockHash,
        },
        depositQueueTransition: IZoneVerifier::DepositQueueTransition {
            prevProcessedHash: output.deposit_queue_transition.prevProcessedHash,
            nextProcessedHash: output.deposit_queue_transition.nextProcessedHash,
            prevDepositNumber: output.deposit_queue_transition.prevDepositNumber,
            nextDepositNumber: output.deposit_queue_transition.nextDepositNumber,
        },
        tokenEnablementTransition: IZoneVerifier::TokenEnablementTransition {
            prevProcessedTokenCount: output.token_enablement_transition.prevProcessedTokenCount,
            nextProcessedTokenCount: output.token_enablement_transition.nextProcessedTokenCount,
        },
        withdrawalQueueHash: output.withdrawal_queue_hash,
        verifierConfig: bundle.verifier_config.clone(),
        proof: bundle.proof.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes};
    use zone_spf::{
        BlockTransition, DepositQueueTransition, LastBatchCommitment, TokenEnablementTransition,
    };

    #[test]
    fn requires_complete_non_debug_measurements() {
        let pcr = "11".repeat(48);
        assert!(
            format!("{pcr},{pcr},{pcr}")
                .parse::<ShadowProofVerifier>()
                .is_ok()
        );
        for invalid in [
            String::new(),
            pcr.clone(),
            format!("{pcr},{pcr}"),
            format!("{pcr},{pcr},{pcr},{pcr}"),
            format!("{pcr},{pcr},00"),
            format!("{pcr},{pcr},{}", "00".repeat(48)),
        ] {
            assert!(invalid.parse::<ShadowProofVerifier>().is_err());
        }
    }

    #[test]
    fn verifier_maps_batch_inputs_and_rejects_fake_proof() {
        let inputs = PublicInputs {
            parent_chain_id: 4217,
            zone_id: 2,
            tempo_block_number: 10,
            anchor_block_number: 11,
            anchor_block_hash: B256::repeat_byte(1),
            expected_withdrawal_batch_index: 12,
        };
        let output = BatchOutput {
            next_zone_height: 13,
            block_transition: BlockTransition {
                prevBlockHash: B256::repeat_byte(2),
                nextBlockHash: B256::repeat_byte(3),
            },
            deposit_queue_transition: DepositQueueTransition {
                prevProcessedHash: B256::repeat_byte(4),
                nextProcessedHash: B256::repeat_byte(5),
                prevDepositNumber: 14,
                nextDepositNumber: 15,
            },
            token_enablement_transition: TokenEnablementTransition {
                prevProcessedTokenCount: 16,
                nextProcessedTokenCount: 17,
            },
            withdrawal_queue_hash: B256::repeat_byte(6),
            last_batch_commitment: LastBatchCommitment {
                withdrawal_batch_index: 12,
            },
        };
        let bundle = ProofBundle {
            verifier_config: Bytes::from_static(&[1]),
            proof: Bytes::from_static(&[1, 2, 3]),
        };
        let call = verifier_call(&inputs, &output, &bundle);
        assert_eq!(call.zoneId, inputs.zone_id);
        assert_eq!(call.tempoBlockNumber, inputs.tempo_block_number);
        assert_eq!(call.anchorBlockNumber, inputs.anchor_block_number);
        assert_eq!(call.anchorBlockHash, inputs.anchor_block_hash);
        assert_eq!(
            call.expectedWithdrawalBatchIndex,
            inputs.expected_withdrawal_batch_index
        );
        assert_eq!(call.nextZoneHeight, U256::from(output.next_zone_height));
        assert_eq!(
            call.blockTransition.prevBlockHash,
            output.block_transition.prevBlockHash
        );
        assert_eq!(
            call.blockTransition.nextBlockHash,
            output.block_transition.nextBlockHash
        );
        assert_eq!(
            call.depositQueueTransition.prevProcessedHash,
            output.deposit_queue_transition.prevProcessedHash
        );
        assert_eq!(
            call.depositQueueTransition.nextProcessedHash,
            output.deposit_queue_transition.nextProcessedHash
        );
        assert_eq!(
            call.depositQueueTransition.prevDepositNumber,
            output.deposit_queue_transition.prevDepositNumber
        );
        assert_eq!(
            call.depositQueueTransition.nextDepositNumber,
            output.deposit_queue_transition.nextDepositNumber
        );
        assert_eq!(
            call.tokenEnablementTransition.prevProcessedTokenCount,
            output.token_enablement_transition.prevProcessedTokenCount
        );
        assert_eq!(
            call.tokenEnablementTransition.nextProcessedTokenCount,
            output.token_enablement_transition.nextProcessedTokenCount
        );
        assert_eq!(call.withdrawalQueueHash, output.withdrawal_queue_hash);
        assert_eq!(call.verifierConfig, bundle.verifier_config);
        assert_eq!(call.proof, bundle.proof);
        let pcr = "11".repeat(48);
        let verifier: ShadowProofVerifier = format!("{pcr},{pcr},{pcr}").parse().unwrap();
        assert!(
            !verifier
                .verify(&inputs, &output, &bundle, 1_800_000_000)
                .unwrap()
        );
    }
}
