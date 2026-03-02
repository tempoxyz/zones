//! Zone-side TIP-403 registry proxy precompile.
//!
//! Deployed at the same address as the L1 [`TIP403Registry`] (`0x403C…0000`), this
//! precompile intercepts external EVM calls to the registry and serves authorization
//! queries from the zone's [`PolicyProvider`] (cache-first, L1 RPC fallback).
//!
//! **Read-only calls** (`isAuthorized`, `isAuthorizedSender`, `isAuthorizedRecipient`,
//! `isAuthorizedMintRecipient`, `policyData`, `compoundPolicyData`, `policyExists`)
//! are resolved cache-first from the [`SharedPolicyCache`]. On cache miss,
//! all queries fall back to L1 RPC (via `block_in_place`) and populate the cache.
//!
//! **Mutating calls** (`createPolicy`, `modifyPolicyWhitelist`, etc.) are reverted —
//! policy state is managed on L1, not on the zone.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_contracts::precompiles::{
    ITIP403Registry::{self, PolicyType},
    TIP403_REGISTRY_ADDRESS,
};
use tracing::{debug, warn};

use crate::l1_state::{
    PolicyProvider,
    tip403::{AuthRole, FIRST_USER_POLICY, POLICY_REJECT_ALL},
};

/// The precompile address — same as the L1 TIP403Registry.
pub const ZONE_TIP403_PROXY_ADDRESS: alloy_primitives::Address = TIP403_REGISTRY_ADDRESS;

/// Fixed gas cost for authorization checks.
const AUTH_CHECK_GAS: u64 = 200;

/// Fixed gas cost for policy data lookups.
const POLICY_DATA_GAS: u64 = 200;

alloy_sol_types::sol! {
    /// Returned when a mutating call is attempted on the read-only zone registry.
    error ReadOnlyRegistry();
}

/// Read-only zone-side proxy for the L1 TIP-403 registry.
///
/// Intercepts EVM calls to the TIP403Registry address and serves authorization
/// queries from the [`SharedPolicyCache`] (populated by the
/// [`PolicyListener`](crate::l1_state::PolicyListener)). All mutating calls
/// (`createPolicy`, `modifyPolicyWhitelist`, etc.) are rejected with
/// `ReadOnlyRegistry` — policy state lives exclusively on L1.
pub struct ZoneTip403ProxyRegistry;

impl ZoneTip403ProxyRegistry {
    /// Create a [`DynPrecompile`] that dispatches TIP-403 registry calls
    /// to the zone's policy cache / L1 RPC fallback.
    pub fn create(provider: PolicyProvider) -> DynPrecompile {
        DynPrecompile::new_stateful(
            PrecompileId::Custom("ZoneTip403ProxyRegistry".into()),
            move |input| {
                if !input.is_direct_call() {
                    warn!(target: "zone::precompile", "ZoneTip403ProxyRegistry called via DELEGATECALL — rejecting");
                    return Ok(PrecompileOutput::new_reverted(
                        0,
                        ReadOnlyRegistry {}.abi_encode().into(),
                    ));
                }

                let data = input.data;
                if data.len() < 4 {
                    warn!(target: "zone::precompile", data_len = data.len(), "ZoneTip403ProxyRegistry called with insufficient data");
                    return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
                }

                let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");

                Self::dispatch(&provider, selector, data)
            },
        )
    }

    /// Dispatch based on the 4-byte selector.
    fn dispatch(provider: &PolicyProvider, selector: [u8; 4], data: &[u8]) -> PrecompileResult {
        // View functions — served from cache/RPC
        if selector == ITIP403Registry::isAuthorizedCall::SELECTOR {
            return Self::handle_is_authorized(provider, data, AuthRole::Transfer);
        }
        if selector == ITIP403Registry::isAuthorizedSenderCall::SELECTOR {
            return Self::handle_is_authorized(provider, data, AuthRole::Sender);
        }
        if selector == ITIP403Registry::isAuthorizedRecipientCall::SELECTOR {
            return Self::handle_is_authorized(provider, data, AuthRole::Recipient);
        }
        if selector == ITIP403Registry::isAuthorizedMintRecipientCall::SELECTOR {
            return Self::handle_is_authorized(provider, data, AuthRole::MintRecipient);
        }
        if selector == ITIP403Registry::policyDataCall::SELECTOR {
            return Self::handle_policy_data(provider, data);
        }
        if selector == ITIP403Registry::compoundPolicyDataCall::SELECTOR {
            return Self::handle_compound_policy_data(provider, data);
        }
        if selector == ITIP403Registry::policyExistsCall::SELECTOR {
            return Self::handle_policy_exists(provider, data);
        }
        if selector == ITIP403Registry::policyIdCounterCall::SELECTOR {
            return Self::handle_policy_id_counter(provider);
        }

        // Mutating functions — all reverted on zone
        if selector == ITIP403Registry::createPolicyCall::SELECTOR
            || selector == ITIP403Registry::createPolicyWithAccountsCall::SELECTOR
            || selector == ITIP403Registry::createCompoundPolicyCall::SELECTOR
            || selector == ITIP403Registry::setPolicyAdminCall::SELECTOR
            || selector == ITIP403Registry::modifyPolicyWhitelistCall::SELECTOR
            || selector == ITIP403Registry::modifyPolicyBlacklistCall::SELECTOR
        {
            debug!(target: "zone::precompile", ?selector, "ZoneTip403ProxyRegistry: mutating call reverted");
            return Ok(PrecompileOutput::new_reverted(
                0,
                ReadOnlyRegistry {}.abi_encode().into(),
            ));
        }

        // Unknown selector
        warn!(target: "zone::precompile", ?selector, "ZoneTip403ProxyRegistry: unknown selector");
        Ok(PrecompileOutput::new_reverted(0, Bytes::new()))
    }

    /// Handle `isAuthorized(policyId, user)` and the directional variants.
    ///
    /// All four share the same ABI shape: `(uint64 policyId, address user) → bool`.
    fn handle_is_authorized(
        provider: &PolicyProvider,
        data: &[u8],
        role: AuthRole,
    ) -> PrecompileResult {
        let call = ITIP403Registry::isAuthorizedCall::abi_decode_raw(&data[4..])
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        let authorized = provider
            .is_authorized_by_policy(call.policyId, call.user, role)
            .map_err(|e| {
                PrecompileError::other(format!(
                    "isAuthorized failed for policy {} user {}: {e}",
                    call.policyId, call.user
                ))
            })?;

        let encoded = ITIP403Registry::isAuthorizedCall::abi_encode_returns(&authorized);
        Ok(PrecompileOutput::new(AUTH_CHECK_GAS, encoded.into()))
    }

    /// Handle `policyData(policyId) → (PolicyType, address admin)`.
    fn handle_policy_data(provider: &PolicyProvider, data: &[u8]) -> PrecompileResult {
        let call = ITIP403Registry::policyDataCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        // Builtins
        if call.policyId < FIRST_USER_POLICY {
            let policy_type = if call.policyId == POLICY_REJECT_ALL {
                PolicyType::WHITELIST
            } else {
                PolicyType::BLACKLIST
            };
            let ret = ITIP403Registry::policyDataReturn {
                policyType: policy_type,
                admin: alloy_primitives::Address::ZERO,
            };
            let encoded = ITIP403Registry::policyDataCall::abi_encode_returns(&ret);
            return Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()));
        }

        // Cache-first, RPC-fallback resolution.
        let policy_type = provider
            .resolve_policy_type_sync(call.policyId)
            .map_err(|e| {
                PrecompileError::other(format!(
                    "policyData failed for policy {}: {e}",
                    call.policyId
                ))
            })?;

        let ret = ITIP403Registry::policyDataReturn {
            policyType: policy_type,
            admin: alloy_primitives::Address::ZERO,
        };
        let encoded = ITIP403Registry::policyDataCall::abi_encode_returns(&ret);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }

    /// Handle `compoundPolicyData(policyId) → (uint64, uint64, uint64)`.
    fn handle_compound_policy_data(provider: &PolicyProvider, data: &[u8]) -> PrecompileResult {
        let call = ITIP403Registry::compoundPolicyDataCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        // Cache-first, RPC-fallback resolution.
        let compound = provider
            .resolve_compound_data_sync(call.policyId)
            .map_err(|e| {
                PrecompileError::other(format!(
                    "compoundPolicyData failed for policy {}: {e}",
                    call.policyId
                ))
            })?;

        let ret = ITIP403Registry::compoundPolicyDataReturn {
            senderPolicyId: compound.sender_policy_id,
            recipientPolicyId: compound.recipient_policy_id,
            mintRecipientPolicyId: compound.mint_recipient_policy_id,
        };
        let encoded = ITIP403Registry::compoundPolicyDataCall::abi_encode_returns(&ret);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }

    /// Handle `policyExists(policyId) → bool`.
    ///
    /// **Cache-only**: returns `true` only for policies observed by the listener.
    /// A `false` result does NOT guarantee the policy doesn't exist on L1.
    fn handle_policy_exists(provider: &PolicyProvider, data: &[u8]) -> PrecompileResult {
        let call = ITIP403Registry::policyExistsCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        // Builtins always exist
        if call.policyId < FIRST_USER_POLICY {
            let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&true);
            return Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()));
        }

        // Check cache for any known policy data
        let exists = provider
            .cache()
            .read()
            .policies()
            .contains_key(&call.policyId);
        let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&exists);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }

    /// Handle `policyIdCounter() → uint64`.
    ///
    /// **Cache-only**: returns the highest cached policy ID + 1 (minimum 2).
    /// This may be lower than the actual L1 counter if the cache is incomplete.
    fn handle_policy_id_counter(provider: &PolicyProvider) -> PrecompileResult {
        let counter = {
            let cache = provider.cache().read();
            cache.policies().keys().max().map_or(2, |max| max + 1)
        };
        let encoded = ITIP403Registry::policyIdCounterCall::abi_encode_returns(&counter);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }
}
