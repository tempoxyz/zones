//! Error types for zone-specific precompiles.

use alloy_sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileOutput, PrecompileResult};
use tempo_precompiles::IntoPrecompileResult;
use tempo_zone_contracts::{ZoneOutboxError, ZonePortalError};

use crate::{
    storage::L1StateError, tip20_factory::ZoneTokenFactoryError, tip403_proxy::ReadOnlyRegistry,
};

pub use tempo_precompiles::error::{Result, TempoPrecompileError};

/// Result type for zone-native precompile operations.
pub type ZoneResult<T> = core::result::Result<T, ZonePrecompileError>;

/// An error raised while executing a zone-specific precompile.
///
/// Upstream Tempo errors retain their original halt/revert/fatal behavior, L1 state failures are
/// fatal, and protocol-specific errors are returned as ABI-encoded EVM reverts.
#[derive(
    Debug, Clone, PartialEq, Eq, thiserror::Error, derive_more::From, derive_more::TryInto,
)]
pub enum ZonePrecompileError {
    /// An error originating in the upstream Tempo precompiles crate.
    #[error(transparent)]
    Tempo(TempoPrecompileError),
    /// A finalized Tempo L1 read failed or conflicted with the active anchor.
    #[error(transparent)]
    L1State(L1StateError),
    /// Error from the ZonePortal.
    #[error("ZonePortal error: {0:?}")]
    Portal(ZonePortalError),
    /// Error from the ZoneOutbox.
    #[error("ZoneOutbox error: {0:?}")]
    Outbox(ZoneOutboxError),
    /// Error from the zone TIP-20 factory.
    #[error("Zone TIP-20 factory error: {0:?}")]
    ZoneTokenFactory(ZoneTokenFactoryError),
    /// Error from the read-only zone TIP-403 registry.
    #[error("Zone TIP-403 registry error: {0:?}")]
    Zone403Registry(ReadOnlyRegistry),
}

impl IntoPrecompileResult for ZonePrecompileError {
    fn into_precompile_result(self, gas: u64, reservoir: u64) -> PrecompileResult {
        let data = match self {
            Self::Tempo(error) => return error.into_precompile_result(gas, reservoir),
            Self::L1State(error) => return Err(error.into()),
            Self::Portal(error) => error.abi_encode(),
            Self::Outbox(error) => error.abi_encode(),
            Self::ZoneTokenFactory(error) => error.abi_encode(),
            Self::Zone403Registry(error) => error.abi_encode(),
        };
        Ok(PrecompileOutput::revert(gas, data.into(), reservoir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolError;
    use revm::precompile::PrecompileHalt;
    use tempo_zone_contracts::IZoneOutbox;

    #[test]
    fn outbox_errors_revert_with_exact_abi_data() {
        for (error, expected) in [
            (
                ZoneOutboxError::GasLimitTooHigh(IZoneOutbox::GasLimitTooHigh {}),
                IZoneOutbox::GasLimitTooHigh {}.abi_encode(),
            ),
            (
                ZoneOutboxError::InvalidWithdrawalCount(IZoneOutbox::InvalidWithdrawalCount {
                    actual: U256::from(1),
                    expected: U256::from(2),
                }),
                IZoneOutbox::InvalidWithdrawalCount {
                    actual: U256::from(1),
                    expected: U256::from(2),
                }
                .abi_encode(),
            ),
        ] {
            let output = ZonePrecompileError::from(error)
                .into_precompile_result(10, 20)
                .unwrap();
            assert!(output.is_revert());
            assert_eq!(output.gas_used, 10);
            assert_eq!(output.reservoir, 20);
            assert_eq!(output.bytes, expected);
        }
    }

    #[test]
    fn other_zone_and_tempo_errors_preserve_conversion_behavior() {
        let factory_error = ZoneTokenFactoryError::only_zone_inbox();
        let output = ZonePrecompileError::from(factory_error.clone())
            .into_precompile_result(10, 20)
            .unwrap();
        assert!(output.is_revert());
        assert_eq!(output.bytes, factory_error.abi_encode());

        let output = ZonePrecompileError::from(TempoPrecompileError::OutOfGas)
            .into_precompile_result(10, 20)
            .unwrap();
        assert_eq!(output.halt_reason(), Some(&PrecompileHalt::OutOfGas));
        assert_eq!(output.reservoir, 20);

        let l1_error = L1StateError::StorageUnavailable {
            account: Address::ZERO,
            slot: B256::ZERO,
            block_number: 1,
            reason: "unavailable".into(),
        };
        assert!(
            ZonePrecompileError::from(l1_error)
                .into_precompile_result(10, 20)
                .is_err(),
            "L1 state failures must remain fatal"
        );
    }
}
