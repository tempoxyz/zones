//! Error types for zone-specific precompiles.

use alloy_sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileOutput, PrecompileResult};
use tempo_precompiles::IntoPrecompileResult;

use crate::{tip20_factory::ZoneTokenFactoryError, tip403_proxy::ReadOnlyRegistry};

// Required by the `#[contract]` proc macro expansion.
pub use tempo_precompiles::error::{Result, TempoPrecompileError};

/// Result type for zone-native precompile operations.
pub type ZoneResult<T> = core::result::Result<T, ZonePrecompileError>;

/// An error raised while executing a zone-specific precompile.
///
/// Upstream Tempo errors retain their original halt/revert/fatal behavior, while each
/// zone-specific error is returned as an ABI-encoded EVM revert.
#[derive(
    Debug, Clone, PartialEq, Eq, thiserror::Error, derive_more::From, derive_more::TryInto,
)]
pub enum ZonePrecompileError {
    /// An error originating in the upstream Tempo precompiles crate.
    #[error(transparent)]
    Tempo(TempoPrecompileError),
    /// Error from the zone TIP-20 factory.
    #[error("Zone TIP-20 factory error: {0:?}")]
    ZoneTokenFactory(ZoneTokenFactoryError),
    /// Error from the zone TIP-403 registry.
    #[error("Zone TIP-403 registry error: {0:?}")]
    Zone403Registry(ReadOnlyRegistry),
}

impl IntoPrecompileResult for ZonePrecompileError {
    #[inline]
    fn into_precompile_result(self, gas: u64, reservoir: u64) -> PrecompileResult {
        let data = match self {
            Self::Tempo(error) => return error.into_precompile_result(gas, reservoir),
            Self::ZoneTokenFactory(error) => error.abi_encode(),
            Self::Zone403Registry(error) => error.abi_encode(),
        };
        Ok(PrecompileOutput::revert(gas, data.into(), reservoir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::precompile::{PrecompileHalt, PrecompileStatus};

    #[test]
    fn zone_errors_revert_with_abi_encoded_data() {
        let factory_error = ZoneTokenFactoryError::only_zone_inbox();
        for (error, expected) in [
            (
                ZonePrecompileError::from(factory_error.clone()),
                factory_error.abi_encode(),
            ),
            (
                ZonePrecompileError::from(ReadOnlyRegistry {}),
                ReadOnlyRegistry {}.abi_encode(),
            ),
        ] {
            let output = error.into_precompile_result(10, 20).unwrap();

            assert!(output.is_revert());
            assert_eq!(output.gas_used, 10);
            assert_eq!(output.reservoir, 20);
            assert_eq!(output.bytes, expected);
        }
    }

    #[test]
    fn tempo_error_preserves_upstream_behavior() {
        let output = ZonePrecompileError::from(TempoPrecompileError::OutOfGas)
            .into_precompile_result(10, 20)
            .unwrap();

        assert!(matches!(
            output.status,
            PrecompileStatus::Halt(PrecompileHalt::OutOfGas)
        ));
        assert_eq!(output.reservoir, 20);
    }
}
