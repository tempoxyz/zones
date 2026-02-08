pub mod dispatch;

use tempo_contracts::precompiles::VERIFIER_ADDRESS;
pub use tempo_contracts::precompiles::IVerifier;
use tempo_precompiles_macros::contract;

use crate::error::Result;

/// Enshrined verifier precompile.
///
/// Stub implementation that always returns `true` for prototyping.
#[contract(addr = VERIFIER_ADDRESS)]
pub struct Verifier {}

impl Verifier {
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    pub fn verify(
        &self,
        _call: IVerifier::verifyCall,
    ) -> Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{StorageCtx, hashmap::HashMapStorageProvider};
    use alloy::primitives::{Address, FixedBytes};

    #[test]
    fn test_verify_always_returns_true() -> eyre::Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, || {
            let verifier = Verifier::new();

            let result = verifier.verify(IVerifier::verifyCall {
                tempoBlockNumber: 1,
                anchorBlockNumber: 1,
                anchorBlockHash: FixedBytes::<32>::ZERO,
                expectedWithdrawalBatchIndex: 1,
                sequencer: Address::ZERO,
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
            })?;

            assert!(result, "Stub verifier should always return true");

            Ok(())
        })
    }
}
