//! Zone-side TIP-403 registry proxy precompile.
//!
//! Deployed at the same address as the L1 [`TIP403Registry`] (`0x403C…0000`), this
//! precompile intercepts external EVM calls to the registry and serves authorization
//! queries from the zone's [`PolicyCheck`] provider (cache-first, L1 RPC fallback).
//!
//! **Read-only calls** (`isAuthorized`, `isAuthorizedSender`, `isAuthorizedRecipient`,
//! `isAuthorizedMintRecipient`, `policyData`, `compoundPolicyData`, `policyExists`)
//! are resolved via the [`PolicyCheck`] trait.
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
use zone_primitives::policy::{AuthRole, FIRST_USER_POLICY, POLICY_REJECT_ALL};

use crate::policy::PolicyCheck;

/// The precompile address — same as the L1 TIP403Registry.
pub const ZONE_TIP403_PROXY_ADDRESS: Address = TIP403_REGISTRY_ADDRESS;

/// Fixed gas cost for authorization checks.
pub const AUTH_CHECK_GAS: u64 = 200;

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
/// at the same address (`0x403C…0000`) and resolves authorization queries via
/// the [`PolicyCheck`] trait.
///
/// All mutating calls (`createPolicy`, `modifyPolicyWhitelist`, etc.) are
/// rejected with `ReadOnlyRegistry` — policy state lives exclusively on L1.
///
/// The struct also exposes [`is_authorized`](Self::is_authorized) and
/// [`is_transfer_authorized`](Self::is_transfer_authorized) for use by the
/// [`ZoneTip20Token`](super::ZoneTip20Token) precompile, which needs the same
/// authorization logic during transfer/mint pre-checks.
#[derive(Debug, Clone)]
pub struct ZoneTip403ProxyRegistry<P> {
    provider: P,
}

impl<P: PolicyCheck> ZoneTip403ProxyRegistry<P> {
    /// Create a new proxy registry backed by the given policy provider.
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Resolve the `transferPolicyId` for a token.
    pub fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64, PrecompileError> {
        self.provider.resolve_transfer_policy_id(token)
    }

    /// Check whether `user` is authorized under `policy_id` for the given `role`.
    pub fn is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> Result<bool, PrecompileError> {
        self.provider.is_authorized(policy_id, user, role)
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
}

impl<P: PolicyCheck + Clone + Send + Sync + 'static> ZoneTip403ProxyRegistry<P> {
    /// Create a [`DynPrecompile`] that dispatches TIP-403 registry calls
    /// to the zone's policy provider.
    pub fn create(provider: P) -> DynPrecompile {
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
}

impl<P: PolicyCheck> ZoneTip403ProxyRegistry<P> {
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

        let policy_type = self.provider.policy_type_sync(call.policyId)?;

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

        let (sender, recipient, mint_recipient) =
            self.provider.compound_policy_data(call.policyId)?;

        let ret = ITIP403Registry::compoundPolicyDataReturn {
            senderPolicyId: sender,
            recipientPolicyId: recipient,
            mintRecipientPolicyId: mint_recipient,
        };
        let encoded = ITIP403Registry::compoundPolicyDataCall::abi_encode_returns(&ret);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }

    /// Handle `policyExists(policyId) → bool`.
    fn handle_policy_exists(&self, data: &[u8]) -> PrecompileResult {
        let call = ITIP403Registry::policyExistsCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        // Builtins always exist
        if call.policyId < FIRST_USER_POLICY {
            let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&true);
            return Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()));
        }

        let exists = self.provider.policy_exists(call.policyId)?;
        let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&exists);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }

    /// Handle `policyIdCounter() → uint64`.
    fn handle_policy_id_counter(&self) -> PrecompileResult {
        let counter = self.provider.policy_id_counter();
        let encoded = ITIP403Registry::policyIdCounterCall::abi_encode_returns(&counter);
        Ok(PrecompileOutput::new(POLICY_DATA_GAS, encoded.into()))
    }
}
