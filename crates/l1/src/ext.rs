//! Extension traits for reading TempoState fields from zone storage.

use alloy_eips::NumHash;
use alloy_primitives::{B256, U256};
use reth_provider::ProviderResult;
use reth_storage_api::StateProvider;

use crate::abi::{TEMPO_BLOCK_HASH_SLOT, TEMPO_PACKED_SLOT, TEMPO_STATE_ADDRESS};

/// Extension trait for reading TempoState fields from zone storage.
pub trait TempoStateExt {
    /// Returns the current `tempoBlockNumber` (the latest L1 block processed by the zone).
    ///
    /// Reads the packed slot 7 of `TempoState` and extracts the lowest `uint64`.
    fn tempo_block_number(&self) -> ProviderResult<u64>;

    /// Returns the current `tempoBlockHash` (the hash of the latest L1 block processed).
    fn tempo_block_hash(&self) -> ProviderResult<B256>;

    /// Returns the current L1 block as a [`NumHash`] (number + hash).
    fn tempo_num_hash(&self) -> ProviderResult<NumHash> {
        Ok(NumHash {
            number: self.tempo_block_number()?,
            hash: self.tempo_block_hash()?,
        })
    }
}

impl<T: StateProvider + ?Sized> TempoStateExt for T {
    fn tempo_block_number(&self) -> ProviderResult<u64> {
        let slot7 = self
            .storage(TEMPO_STATE_ADDRESS, TEMPO_PACKED_SLOT)?
            .unwrap_or_default();
        Ok((slot7 & U256::from(u64::MAX)).to::<u64>())
    }

    fn tempo_block_hash(&self) -> ProviderResult<B256> {
        Ok(self
            .storage(TEMPO_STATE_ADDRESS, TEMPO_BLOCK_HASH_SLOT)?
            .map(|v| B256::from(v.to_be_bytes()))
            .unwrap_or_default())
    }
}

/// Extension trait for reading TempoState fields from a [`reth_provider::Chain`]'s execution
/// outcome.
///
/// Separate from [`TempoStateExt`] because `Chain` does not implement [`StateProvider`] — it
/// reads from the bundled [`ExecutionOutcome`](reth_provider::ExecutionOutcome) instead.
pub trait ChainTempoStateExt {
    /// Returns the current `tempoBlockNumber` from the chain's execution outcome.
    fn tempo_block_number(&self) -> u64;

    /// Returns the current `tempoBlockHash` from the chain's execution outcome.
    fn tempo_block_hash(&self) -> B256;

    /// Returns the current L1 block as a [`NumHash`] (number + hash).
    fn tempo_num_hash(&self) -> NumHash {
        NumHash {
            number: self.tempo_block_number(),
            hash: self.tempo_block_hash(),
        }
    }
}

impl<N: reth_primitives_traits::NodePrimitives> ChainTempoStateExt for reth_provider::Chain<N> {
    fn tempo_block_number(&self) -> u64 {
        let slot7 = self
            .execution_outcome()
            .storage(
                &TEMPO_STATE_ADDRESS,
                U256::from_be_bytes(TEMPO_PACKED_SLOT.0),
            )
            .unwrap_or_default();
        (slot7 & U256::from(u64::MAX)).to::<u64>()
    }

    fn tempo_block_hash(&self) -> B256 {
        self.execution_outcome()
            .storage(
                &TEMPO_STATE_ADDRESS,
                U256::from_be_bytes(TEMPO_BLOCK_HASH_SLOT.0),
            )
            .map(|v| B256::from(v.to_be_bytes()))
            .unwrap_or_default()
    }
}
