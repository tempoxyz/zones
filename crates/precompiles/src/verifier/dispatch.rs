use super::Verifier;
use crate::{Precompile, dispatch_call, input_cost, view};
use alloy::{primitives::Address, sol_types::SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};
use tempo_contracts::precompiles::IVerifier::IVerifierCalls;

impl Precompile for Verifier {
    fn call(&mut self, calldata: &[u8], _msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(calldata, IVerifierCalls::abi_decode, |call| match call {
            IVerifierCalls::verify(call) => view(call, |c| self.verify(c)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        storage::{StorageCtx, hashmap::HashMapStorageProvider},
        test_util::{assert_full_coverage, check_selector_coverage},
    };
    use alloy::{
        primitives::{Address, FixedBytes},
        sol_types::{SolCall, SolValue},
    };
    use tempo_contracts::precompiles::IVerifier;

    #[test]
    fn test_verify_dispatch() -> eyre::Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, || {
            let mut verifier = Verifier::new();

            let verify_call = IVerifier::verifyCall {
                tempoBlockNumber: 42,
                anchorBlockNumber: 42,
                anchorBlockHash: FixedBytes::<32>::ZERO,
                expectedWithdrawalBatchIndex: 1,
                sequencer: Address::random(),
                blockTransition: IVerifier::BlockTransition {
                    prevBlockHash: FixedBytes::<32>::ZERO,
                    nextBlockHash: FixedBytes::<32>::ZERO,
                },
                depositQueueTransition: IVerifier::DepositQueueTransition {
                    prevProcessedHash: FixedBytes::<32>::ZERO,
                    nextProcessedHash: FixedBytes::<32>::ZERO,
                },
                withdrawalQueueTransition: IVerifier::WithdrawalQueueTransition {
                    withdrawalQueueHash: FixedBytes::<32>::ZERO,
                },
                verifierConfig: Default::default(),
                proof: Default::default(),
            };
            let calldata = verify_call.abi_encode();

            let result = verifier.call(&calldata, Address::random())?;
            assert!(!result.reverted);

            let decoded = bool::abi_decode(&result.bytes)?;
            assert!(decoded);

            Ok(())
        })
    }

    #[test]
    fn test_invalid_selector() -> eyre::Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, || {
            let mut verifier = Verifier::new();

            let result = verifier.call(&[0x12, 0x34, 0x56, 0x78], Address::random())?;
            assert!(result.reverted);

            Ok(())
        })
    }

    #[test]
    fn test_selector_coverage() -> eyre::Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, || {
            let mut verifier = Verifier::new();

            let unsupported = check_selector_coverage(
                &mut verifier,
                IVerifierCalls::SELECTORS,
                "IVerifier",
                IVerifierCalls::name_by_selector,
            );

            assert_full_coverage([unsupported]);

            Ok(())
        })
    }
}
