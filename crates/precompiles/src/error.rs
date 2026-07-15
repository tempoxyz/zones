//! Error types for zone-specific precompiles.

use alloy_sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileOutput, PrecompileResult};
use tempo_precompiles::IntoPrecompileResult;

use crate::{tip20_factory::ZoneTokenFactoryError, tip403_proxy::ReadOnlyRegistry};

// Required by the `#[contract]` proc macro expansion.
pub use tempo_precompiles::error::{Result, TempoPrecompileError};

/// An error raised while executing a zone-specific precompile.
///
/// Upstream Tempo errors retain their original halt/revert/fatal behavior, while
/// each zone-specific error is returned as an ABI-encoded EVM revert.
#[derive(Clone)]
pub enum ZonePrecompileError {
    /// An error originating in the upstream Tempo precompiles crate.
    Tempo(TempoPrecompileError),
    /// The zone TIP-20 factory was called by an address other than the zone inbox.
    ZoneTokenFactory(ZoneTokenFactoryError),
    /// A mutating call was attempted on the read-only zone TIP-403 registry.
    Zone403Registry(ReadOnlyRegistry),
}

impl From<TempoPrecompileError> for ZonePrecompileError {
    #[inline]
    fn from(error: TempoPrecompileError) -> Self {
        Self::Tempo(error)
    }
}

impl From<ZoneTokenFactoryError> for ZonePrecompileError {
    #[inline]
    fn from(error: ZoneTokenFactoryError) -> Self {
        Self::ZoneTokenFactory(error)
    }
}

impl From<ReadOnlyRegistry> for ZonePrecompileError {
    #[inline]
    fn from(error: ReadOnlyRegistry) -> Self {
        Self::Zone403Registry(error)
    }
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
