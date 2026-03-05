//! Zone-specific TIP-20 token precompile with PolicyProvider-backed authorization.
//!
//! On L1, the vanilla [`TIP20Token`] checks transfer/mint authorization by
//! instantiating a `TIP403Registry` in Rust which reads EVM storage at
//! `0x403C…0000`. On the zone, that storage is empty (defaults to policy 1 =
//! allow-all), so all transfers pass regardless of L1 blacklists.
//!
//! This wrapper intercepts transfer and mint calls, checks authorization
//! against the zone's [`ZoneTip403ProxyRegistry`] (which delegates to
//! [`PolicyProvider`] — cache-first, L1 RPC fallback), and only then delegates
//! to the vanilla `TIP20Token` implementation. The vanilla call's internal
//! `TIP403Registry::new()` check still runs but always passes (empty storage →
//! allow-all), so the real enforcement has already happened.
//!
//! NOTE: This is a temporary solution until the vanilla TIP-20 implementation
//! is made configurable to accept an external authorization provider.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput};
use tempo_precompiles::{
    DelegateCallNotAllowed, Precompile as TempoPrecompile,
    storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
    tip20::{ITIP20, TIP20Token},
};
use tracing::{debug, trace};

use super::tip403_proxy::{AUTH_CHECK_GAS, ZoneTip403ProxyRegistry};
use crate::l1_state::tip403::AuthRole;

/// Decode ABI args or return a reverted precompile output.
///
/// Unlike `.ok()?` (which silently skips the policy check on decode failure),
/// this macro returns a definitive revert so malformed calldata cannot bypass
/// the zone policy layer.
macro_rules! decode_or_revert {
    ($call_ty:ty, $args:expr) => {
        match <$call_ty>::abi_decode_raw($args) {
            Ok(c) => c,
            Err(_) => {
                return Some(Ok(PrecompileOutput::new_reverted(0, Bytes::new())));
            }
        }
    };
}

/// Zone-specific TIP-20 token precompile.
///
/// Wraps the vanilla [`TIP20Token`] and the [`ZoneTip403ProxyRegistry`] to add
/// PolicyProvider-backed authorization for transfers and mints. All other calls
/// (balanceOf, approve, metadata, roles, rewards, etc.) are passed through
/// unmodified to the vanilla implementation.
pub struct ZoneTip20Token {
    registry: ZoneTip403ProxyRegistry,
}

impl ZoneTip20Token {
    /// Create a new wrapper with the given registry.
    pub fn new(registry: ZoneTip403ProxyRegistry) -> Self {
        Self { registry }
    }

    /// Create a [`DynPrecompile`] for a zone-side TIP-20 token at `address`.
    ///
    /// The returned precompile:
    /// 1. Checks the 4-byte selector for transfer/mint calls.
    /// 2. For those calls, reads `transfer_policy_id` from EVM storage and
    ///    checks authorization via the [`ZoneTip403ProxyRegistry`].
    /// 3. Delegates to the vanilla `TIP20Token::call()` for execution.
    pub fn create(
        address: Address,
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
        registry: ZoneTip403ProxyRegistry,
    ) -> DynPrecompile {
        let spec = cfg.spec;
        let gas_params = cfg.gas_params.clone();
        let token = Self::new(registry);

        DynPrecompile::new_stateful(
            PrecompileId::Custom("ZoneTip20Token".into()),
            move |input| {
                if !input.is_direct_call() {
                    return Ok(PrecompileOutput::new_reverted(
                        0,
                        SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
                    ));
                }

                let mut storage = EvmPrecompileStorageProvider::new(
                    input.internals,
                    input.gas,
                    spec,
                    input.is_static,
                    gas_params.clone(),
                );

                StorageCtx::enter(&mut storage, || {
                    // Pre-check: enforce zone policy for transfer/mint calls.
                    // Returns Some(reverted output) if policy forbids, None if allowed.
                    if let Some(revert) = token.check_policy(address, input.data, input.caller) {
                        return revert;
                    }

                    // Policy passed (or non-transfer call) — delegate to vanilla TIP20Token
                    let mut tip20 =
                        TIP20Token::from_address(address).expect("TIP20 prefix already verified");
                    tip20.call(input.data, input.caller)
                })
            },
        )
    }

    /// Check policy authorization for transfer/mint selectors.
    ///
    /// Returns `Some(Ok(reverted_output))` if the call is forbidden by policy.
    /// Returns `None` if the call is allowed or is not a transfer/mint.
    fn check_policy(
        &self,
        address: Address,
        data: &[u8],
        caller: Address,
    ) -> Option<revm::precompile::PrecompileResult> {
        if data.len() < 4 {
            return None;
        }

        let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");
        let args = &data[4..];

        match selector {
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
                let call = decode_or_revert!(ITIP20::mintCall, args);
                self.enforce_mint(address, call.to)
            }
            ITIP20::mintWithMemoCall::SELECTOR => {
                let call = decode_or_revert!(ITIP20::mintWithMemoCall, args);
                self.enforce_mint(address, call.to)
            }
            _ => None,
        }
    }

    /// Check sender + recipient authorization for a transfer.
    ///
    /// Returns `Some(revert)` if forbidden, `None` if allowed.
    fn enforce_transfer(
        &self,
        token: Address,
        from: Address,
        to: Address,
    ) -> Option<revm::precompile::PrecompileResult> {
        let policy_id = match self.resolve_transfer_policy_id(token) {
            Ok(id) => id,
            Err(e) => {
                // Can't resolve policy — token may be uninitialized or RPC
                // unreachable. Fall through to vanilla TIP20Token which will
                // handle it (revert for uninitialized, or read from EVM storage).
                debug!(
                    target: "zone::precompile",
                    %token, error = %e,
                    "failed to resolve transfer_policy_id, deferring to vanilla TIP20"
                );
                return None;
            }
        };

        trace!(
            target: "zone::precompile",
            %token, %from, %to, policy_id,
            "ZoneTip20Token: checking transfer authorization"
        );

        match self.registry.is_transfer_authorized(policy_id, from, to) {
            Ok(true) => None,
            Ok(false) => {
                trace!(
                    target: "zone::precompile",
                    %from, %to, policy_id, "transfer not authorized"
                );
                Some(Ok(Self::policy_forbids_output()))
            }
            Err(e) => Some(Err(e)),
        }
    }

    /// Check mint recipient authorization.
    ///
    /// Returns `Some(revert)` if forbidden, `None` if allowed.
    fn enforce_mint(
        &self,
        token: Address,
        to: Address,
    ) -> Option<revm::precompile::PrecompileResult> {
        let policy_id = match self.resolve_transfer_policy_id(token) {
            Ok(id) => id,
            Err(e) => {
                debug!(
                    target: "zone::precompile",
                    %token, error = %e,
                    "failed to resolve transfer_policy_id, deferring to vanilla TIP20"
                );
                return None;
            }
        };

        trace!(
            target: "zone::precompile",
            %token, %to, policy_id,
            "ZoneTip20Token: checking mint recipient authorization"
        );

        match self
            .registry
            .is_authorized(policy_id, to, AuthRole::MintRecipient)
        {
            Ok(true) => None,
            Ok(false) => {
                trace!(target: "zone::precompile", %to, policy_id, "mint recipient not authorized");
                Some(Ok(Self::policy_forbids_output()))
            }
            Err(e) => Some(Err(e)),
        }
    }

    /// Resolve the `transfer_policy_id` for a token — cache first, L1 RPC fallback.
    fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64, PrecompileError> {
        self.registry.resolve_transfer_policy_id(token)
    }

    /// Build a reverted output with the `policyForbids()` error selector.
    fn policy_forbids_output() -> PrecompileOutput {
        PrecompileOutput::new_reverted(
            AUTH_CHECK_GAS,
            tempo_contracts::precompiles::TIP20Error::policy_forbids()
                .selector()
                .into(),
        )
    }
}
