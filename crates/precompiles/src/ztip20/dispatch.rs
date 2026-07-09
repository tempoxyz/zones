//! ABI dispatch and precheck routing for the [`ZoneTip20Token`] wrapper.

use alloc::sync::Arc;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_precompiles::{
    DelegateCallNotAllowed, Precompile as TempoPrecompile, charge_input_cost,
    dispatch::selector_from_calldata,
    storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
    tip20::{IRolesAuth, ITIP20},
};

use super::{FIXED_TRANSFER_GAS, PrecheckErr, PrecheckResult, SequencerExt, ZoneTip20Token};
use crate::{policy::PolicyCheck, tip403_proxy::ZoneTip403ProxyRegistry};

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

/// Decode ABI args or request a definitive revert.
///
/// Unlike `.ok()?` (which would silently skip the policy check on failure), returns a precheck
/// abort so malformed calldata cannot bypass the zone policy layer for intercepted selectors.
fn decode_or_revert<C: SolCall>(args: &[u8]) -> Result<C, PrecheckErr> {
    C::abi_decode_raw_validate(args)
        .map_err(|_| PrecheckErr::revert(StorageCtx::default().revert_output(Bytes::new())))
}

impl<P: PolicyCheck> ZoneTip20Token<P> {
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

    /// Check selector-specific privacy, policy, and bridge-path rules.
    fn precheck(&self, selector: [u8; 4], data: &[u8], caller: Address) -> PrecheckResult {
        let args = &data[4..];

        match selector {
            ITIP20::balanceOfCall::SELECTOR => {
                let call = decode_or_revert::<ITIP20::balanceOfCall>(args)?;
                self.check_balance_read(call.account, caller)
            }
            ITIP20::allowanceCall::SELECTOR => {
                let call = decode_or_revert::<ITIP20::allowanceCall>(args)?;
                self.check_allowance_read(call.owner, call.spender, caller)
            }
            ITIP20::transferCall::SELECTOR => {
                let call = decode_or_revert::<ITIP20::transferCall>(args)?;
                self.check_transfer_policy(caller, call.to)
            }
            ITIP20::transferFromCall::SELECTOR => {
                let call = decode_or_revert::<ITIP20::transferFromCall>(args)?;
                self.check_transfer_policy(call.from, call.to)
            }
            ITIP20::transferWithMemoCall::SELECTOR => {
                let call = decode_or_revert::<ITIP20::transferWithMemoCall>(args)?;
                self.check_transfer_policy(caller, call.to)
            }
            ITIP20::transferFromWithMemoCall::SELECTOR => {
                let call = decode_or_revert::<ITIP20::transferFromWithMemoCall>(args)?;
                self.check_transfer_policy(call.from, call.to)
            }
            ITIP20::mintCall::SELECTOR => {
                self.check_mint_auth(caller)?;
                let call = decode_or_revert::<ITIP20::mintCall>(args)?;
                self.check_mint_recipient_policy(call.to)
            }
            ITIP20::mintWithMemoCall::SELECTOR => {
                self.check_mint_auth(caller)?;
                let call = decode_or_revert::<ITIP20::mintWithMemoCall>(args)?;
                self.check_mint_recipient_policy(call.to)
            }
            ITIP20::burnCall::SELECTOR | ITIP20::burnWithMemoCall::SELECTOR => {
                self.check_burn_auth(caller)
            }
            ITIP20::userRewardInfoCall::SELECTOR => {
                let call = decode_or_revert::<ITIP20::userRewardInfoCall>(args)?;
                self.check_balance_read(call.account, caller)
            }
            ITIP20::getPendingRewardsCall::SELECTOR => {
                let call = decode_or_revert::<ITIP20::getPendingRewardsCall>(args)?;
                self.check_balance_read(call.account, caller)
            }
            IRolesAuth::hasRoleCall::SELECTOR => {
                let call = decode_or_revert::<IRolesAuth::hasRoleCall>(args)?;
                self.check_balance_read(call.account, caller)
            }
            _ => Ok(()),
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
        let token = Self::new(address, registry, sequencer);

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

                let selector = selector_from_calldata(input.data);
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

                    if let Err(err) = token.ensure_initialized() {
                        return finish(add_input_cost(input.data, storage.error_result(err)));
                    }

                    if let Some(selector) = selector
                        && let Err(abort) = token.precheck(selector, input.data, input.caller)
                    {
                        return finish(add_input_cost(input.data, abort.into()));
                    };

                    finish(token.tip20().call(input.data, input.caller))
                })
            },
        )
    }
}
