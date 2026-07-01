//! ABI dispatch and precheck routing for the [`ZoneTip20Token`] wrapper.

use alloc::sync::Arc;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_precompiles::{
    DelegateCallNotAllowed, Precompile as TempoPrecompile, charge_input_cost,
    storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
    tip20::{IRolesAuth, ITIP20, TIP20Token},
};

use super::{FIXED_TRANSFER_GAS, SequencerExt, ZoneTip20Token};
use crate::{policy::PolicyCheck, tip403_proxy::ZoneTip403ProxyRegistry};

/// Decode ABI args or return a reverted precompile output.
///
/// Unlike `.ok()?` (which silently skips the policy check on decode failure),
/// this macro returns a definitive revert so malformed calldata cannot bypass
/// the zone policy layer.
macro_rules! decode_or_revert {
    ($call_ty:ty, $args:expr) => {
        match <$call_ty>::abi_decode_raw_validate($args) {
            Ok(c) => c,
            Err(_) => {
                return Some(Ok(StorageCtx::default().revert_output(Bytes::new())));
            }
        }
    };
}

impl<P: PolicyCheck> ZoneTip20Token<P> {
    fn add_input_cost(calldata: &[u8], result: PrecompileResult) -> PrecompileResult {
        let mut storage = StorageCtx::default();
        let gas_before = storage.gas_used();
        if let Some(err) = charge_input_cost(&mut storage, calldata) {
            return err;
        }
        let input_gas = storage.gas_used().saturating_sub(gas_before);

        result.map(|mut output| {
            output.gas_used = output.gas_used.saturating_add(input_gas);
            output
        })
    }

    fn selector(data: &[u8]) -> Option<[u8; 4]> {
        tempo_precompiles::dispatch::selector_from_calldata(data)
    }

    fn is_fixed_gas_selector(selector: [u8; 4]) -> bool {
        matches!(
            selector,
            ITIP20::transferCall::SELECTOR
                | ITIP20::transferFromCall::SELECTOR
                | ITIP20::transferWithMemoCall::SELECTOR
                | ITIP20::transferFromWithMemoCall::SELECTOR
                | ITIP20::approveCall::SELECTOR
        )
    }

    fn apply_fixed_gas(result: PrecompileResult) -> PrecompileResult {
        match result {
            Ok(mut output) => {
                output.gas_used = FIXED_TRANSFER_GAS;
                Ok(output)
            }
            Err(err) => Err(err),
        }
    }

    /// Check selector-specific privacy/auth rules before delegating.
    ///
    /// Returns `Some(Ok(reverted_output))` if the call is forbidden.
    /// Returns `None` if the call may delegate to vanilla TIP20.
    fn precheck(
        &self,
        selector: [u8; 4],
        address: Address,
        data: &[u8],
        caller: Address,
    ) -> Option<PrecompileResult> {
        let args = &data[4..];

        match selector {
            ITIP20::balanceOfCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::balanceOfCall, args);
                self.enforce_balance_of(call.account, caller)
            }
            ITIP20::allowanceCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::allowanceCall, args);
                self.enforce_allowance(call.owner, call.spender, caller)
            }
            ITIP20::transferCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::transferCall, args);
                self.enforce_transfer(address, caller, call.to)
            }
            ITIP20::transferFromCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::transferFromCall, args);
                self.enforce_transfer(address, call.from, call.to)
            }
            ITIP20::transferWithMemoCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::transferWithMemoCall, args);
                self.enforce_transfer(address, caller, call.to)
            }
            ITIP20::transferFromWithMemoCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::transferFromWithMemoCall, args);
                self.enforce_transfer(address, call.from, call.to)
            }
            ITIP20::mintCall::SELECTOR => {
                if let Some(revert) = self.reject_crossed_mint_caller(caller) {
                    return Some(revert);
                }
                let call = decode_or_revert!(ITIP20::mintCall, args);
                self.enforce_mint(address, call.to)
            }
            ITIP20::mintWithMemoCall::SELECTOR => {
                if let Some(revert) = self.reject_crossed_mint_caller(caller) {
                    return Some(revert);
                }
                let call = decode_or_revert!(ITIP20::mintWithMemoCall, args);
                self.enforce_mint(address, call.to)
            }
            ITIP20::burnCall::SELECTOR | ITIP20::burnWithMemoCall::SELECTOR => {
                self.reject_crossed_burn_caller(caller)
            }
            ITIP20::userRewardInfoCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::userRewardInfoCall, args);
                self.enforce_balance_of(call.account, caller)
            }
            ITIP20::getPendingRewardsCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::getPendingRewardsCall, args);
                self.enforce_balance_of(call.account, caller)
            }
            IRolesAuth::hasRoleCall::SELECTOR => {
                let call = decode_or_revert!(IRolesAuth::hasRoleCall, args);
                self.enforce_balance_of(call.account, caller)
            }
            _ => None,
        }
    }
}

impl<P> ZoneTip20Token<P>
where
    P: PolicyCheck + Clone + Send + Sync + 'static,
{
    /// Create a [`DynPrecompile`] for a zone-side TIP-20 token at `address`.
    ///
    /// The returned precompile:
    /// 1. Rejects uninitialized TIP-20-prefix addresses.
    /// 2. Checks the 4-byte selector for transfer/mint calls.
    /// 3. When a TIP-403 registry is configured, reads `transfer_policy_id`
    ///    from EVM storage and checks authorization via the
    ///    [`ZoneTip403ProxyRegistry`].
    /// 4. Delegates to the vanilla `TIP20Token::call()` for execution.
    pub fn create(
        address: Address,
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
        registry: Option<ZoneTip403ProxyRegistry<P>>,
        sequencer: Arc<dyn SequencerExt>,
    ) -> DynPrecompile {
        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();
        let token = Self::new(registry, sequencer);

        DynPrecompile::new_stateful(
            PrecompileId::Custom("ZoneTip20Token".into()),
            move |input| {
                if !input.is_direct_call() {
                    return Ok(PrecompileOutput::revert(
                        0,
                        SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
                        input.reservoir,
                    ));
                }

                let selector = Self::selector(input.data);
                let is_fixed_gas = selector.is_some_and(Self::is_fixed_gas_selector);
                if is_fixed_gas && input.gas < FIXED_TRANSFER_GAS {
                    return Ok(PrecompileOutput::halt(
                        PrecompileHalt::OutOfGas,
                        input.reservoir,
                    ));
                }

                let mut storage = EvmPrecompileStorageProvider::new(
                    input.internals,
                    if is_fixed_gas { u64::MAX } else { input.gas },
                    input.reservoir,
                    spec,
                    amsterdam_eip8037_enabled,
                    input.is_static,
                    gas_params.clone(),
                );

                StorageCtx::enter(&mut storage, || {
                    let storage = StorageCtx::default();
                    let finish = |result| {
                        if is_fixed_gas {
                            Self::apply_fixed_gas(result)
                        } else {
                            result
                        }
                    };

                    let mut tip20 =
                        TIP20Token::from_address(address).expect("TIP20 prefix already verified");

                    if let Err(err) = Self::ensure_initialized(&tip20) {
                        return finish(Self::add_input_cost(input.data, storage.error_result(err)));
                    }

                    if let Some(selector) = selector
                        && let Some(revert) =
                            token.precheck(selector, address, input.data, input.caller)
                    {
                        return finish(Self::add_input_cost(input.data, revert));
                    }

                    finish(tip20.call(input.data, input.caller))
                })
            },
        )
    }
}
