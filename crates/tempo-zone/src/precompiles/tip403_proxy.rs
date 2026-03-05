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
use alloy_primitives::{Address, Bytes};
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
pub const ZONE_TIP403_PROXY_ADDRESS: Address = TIP403_REGISTRY_ADDRESS;

/// Fixed gas cost for authorization checks.
pub(crate) const AUTH_CHECK_GAS: u64 = 200;

/// Fixed gas cost for policy data lookups.
const POLICY_DATA_GAS: u64 = 200;

alloy_sol_types::sol! {
    /// Returned when a mutating call is attempted on the read-only zone registry.
    error ReadOnlyRegistry();
}

/// Read-only zone-side proxy that mirrors the L1 TIP-403 registry.
///
/// Unlike the L1 [`TIP403Registry`] (which is a storage-backed `#[contract]`
/// precompile), this proxy has **no on-chain storage**. It intercepts EVM calls
/// at the same address (`0x403C…0000`) and resolves authorization queries from
/// the in-memory [`PolicyProvider`] (cache-first, L1 RPC fallback). The
/// underlying cache is populated by the [`PolicyListener`](crate::l1_state::PolicyListener)
/// which streams events from L1.
///
/// All mutating calls (`createPolicy`, `modifyPolicyWhitelist`, etc.) are
/// rejected with `ReadOnlyRegistry` — policy state lives exclusively on L1.
///
/// Genesis initializes the L1 `TIP403Registry` at this address solely to set
/// `0xef` bytecode so Solidity's `EXTCODESIZE` guard passes; the storage it
/// writes is unused.
///
/// The struct also exposes [`is_authorized`](Self::is_authorized) and
/// [`is_transfer_authorized`](Self::is_transfer_authorized) for use by the
/// [`ZoneTip20Token`](super::ZoneTip20Token) precompile, which needs the same
/// authorization logic during transfer/mint pre-checks.
#[derive(Debug, Clone)]
pub struct ZoneTip403ProxyRegistry {
    provider: PolicyProvider,
}

impl ZoneTip403ProxyRegistry {
    /// Create a new proxy registry backed by the given policy provider.
    pub fn new(provider: PolicyProvider) -> Self {
        Self { provider }
    }

    /// Resolve the `transferPolicyId` for a token — cache first, RPC fallback.
    pub fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64, PrecompileError> {
        self.provider
            .resolve_transfer_policy_id(token)
            .map_err(|e| {
                PrecompileError::other(format!(
                    "failed to resolve transfer_policy_id for {token}: {e}"
                ))
            })
    }

    /// Check whether `user` is authorized under `policy_id` for the given `role`.
    pub fn is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> Result<bool, PrecompileError> {
        self.provider
            .is_authorized_by_policy(policy_id, user, role)
            .map_err(|e| {
                PrecompileError::other(format!(
                    "auth check failed for policy {policy_id} user {user}: {e}"
                ))
            })
    }

    /// Check sender + recipient authorization for a transfer.
    ///
    /// Short-circuits on sender failure (matching L1 T2 behavior).
    pub fn is_transfer_authorized(
        &self,
        policy_id: u64,
        from: Address,
        to: Address,
    ) -> Result<bool, PrecompileError> {
        if !self.is_authorized(policy_id, from, AuthRole::Sender)? {
            return Ok(false);
        }
        self.is_authorized(policy_id, to, AuthRole::Recipient)
    }

    /// Create a [`DynPrecompile`] that dispatches TIP-403 registry calls
    /// to the zone's policy cache / L1 RPC fallback.
    pub fn create(provider: PolicyProvider) -> DynPrecompile {
        let registry = Self::new(provider);
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

                registry.dispatch(selector, data)
            },
        )
    }

    /// Dispatch based on the 4-byte selector.
    fn dispatch(&self, selector: [u8; 4], data: &[u8]) -> PrecompileResult {
        // View functions — served from cache/RPC
        if selector == ITIP403Registry::isAuthorizedCall::SELECTOR {
            return self.handle_is_authorized(data, AuthRole::Transfer);
        }
        if selector == ITIP403Registry::isAuthorizedSenderCall::SELECTOR {
            return self.handle_is_authorized(data, AuthRole::Sender);
        }
        if selector == ITIP403Registry::isAuthorizedRecipientCall::SELECTOR {
            return self.handle_is_authorized(data, AuthRole::Recipient);
        }
        if selector == ITIP403Registry::isAuthorizedMintRecipientCall::SELECTOR {
            return self.handle_is_authorized(data, AuthRole::MintRecipient);
        }
        if selector == ITIP403Registry::policyDataCall::SELECTOR {
            return self.handle_policy_data(data);
        }
        if selector == ITIP403Registry::compoundPolicyDataCall::SELECTOR {
            return self.handle_compound_policy_data(data);
        }
        if selector == ITIP403Registry::policyExistsCall::SELECTOR {
            return self.handle_policy_exists(data);
        }
        if selector == ITIP403Registry::policyIdCounterCall::SELECTOR {
            return self.handle_policy_id_counter();
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
    fn handle_is_authorized(&self, data: &[u8], role: AuthRole) -> PrecompileResult {
        let call = ITIP403Registry::isAuthorizedCall::abi_decode_raw(&data[4..])
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        let authorized = self.is_authorized(call.policyId, call.user, role)?;

        let encoded = ITIP403Registry::isAuthorizedCall::abi_encode_returns(&authorized);
        Ok(PrecompileOutput::new(AUTH_CHECK_GAS, encoded.into()))
    }

    /// Handle `policyData(policyId) → (PolicyType, address admin)`.
    fn handle_policy_data(&self, data: &[u8]) -> PrecompileResult {
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
                admin: Address::ZERO,
            };
            let encoded = ITIP403Registry::policyDataCall::abi_encode_returns(&ret);
            return Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()));
        }

        // Cache-first, RPC-fallback resolution.
        let policy_type = self
            .provider
            .resolve_policy_type_sync(call.policyId)
            .map_err(|e| {
                PrecompileError::other(format!(
                    "policyData failed for policy {}: {e}",
                    call.policyId
                ))
            })?;

        let ret = ITIP403Registry::policyDataReturn {
            policyType: policy_type,
            admin: Address::ZERO,
        };
        let encoded = ITIP403Registry::policyDataCall::abi_encode_returns(&ret);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }

    /// Handle `compoundPolicyData(policyId) → (uint64, uint64, uint64)`.
    fn handle_compound_policy_data(&self, data: &[u8]) -> PrecompileResult {
        let call = ITIP403Registry::compoundPolicyDataCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        // Cache-first, RPC-fallback resolution.
        let compound = self
            .provider
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
    /// Cache-first, RPC-fallback: attempts to resolve the policy type via
    /// [`PolicyProvider::resolve_policy_type_sync`]. If resolution succeeds
    /// the policy exists; if the RPC call itself fails, the error propagates.
    fn handle_policy_exists(&self, data: &[u8]) -> PrecompileResult {
        let call = ITIP403Registry::policyExistsCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        // Builtins always exist
        if call.policyId < FIRST_USER_POLICY {
            let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&true);
            return Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()));
        }

        // Cache-first, RPC-fallback — reuse policy type resolution.
        let exists = match self.provider.resolve_policy_type_sync(call.policyId) {
            Ok(_) => true,
            Err(_) => false,
        };
        let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&exists);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }

    /// Handle `policyIdCounter() → uint64`.
    ///
    /// **Cache-only**: returns the highest cached policy ID + 1 (minimum 2).
    /// This may be lower than the actual L1 counter if the cache is incomplete.
    fn handle_policy_id_counter(&self) -> PrecompileResult {
        let counter = {
            let cache = self.provider.cache().read();
            cache.policies().keys().max().map_or(2, |max| max + 1)
        };
        let encoded = ITIP403Registry::policyIdCounterCall::abi_encode_returns(&counter);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }
}
