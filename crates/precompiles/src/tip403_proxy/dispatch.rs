//! ABI dispatch for the [`ZoneTip403ProxyRegistry`] precompile.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError, SolInterface};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_contracts::precompiles::ITIP403Registry::{self, PolicyType};
use tempo_precompiles::tip403_registry::{ALLOW_ALL_POLICY_ID, REJECT_ALL_POLICY_ID};
use tracing::{debug, warn};
use zone_primitives::policy::AuthRole;

use super::{POLICY_DATA_GAS, ReadOnlyRegistry, ZoneTip403ProxyRegistry};
use crate::policy::PolicyCheck;

impl<P: PolicyCheck + Clone + Send + Sync + 'static> ZoneTip403ProxyRegistry<P> {
    /// Create a [`DynPrecompile`] that dispatches TIP-403 registry calls
    /// to the zone's policy provider.
    pub fn create(provider: P) -> DynPrecompile {
        let registry = Self::new(provider);
        DynPrecompile::new_stateful(
            PrecompileId::Custom("ZoneTip403ProxyRegistry".into()),
            move |input| {
                if !input.is_direct_call() {
                    warn!(
                        target: "zone::precompile",
                        "ZoneTip403ProxyRegistry called via DELEGATECALL - rejecting"
                    );
                    return Ok(PrecompileOutput::revert(
                        0,
                        ReadOnlyRegistry {}.abi_encode().into(),
                        input.reservoir,
                    ));
                }

                registry.dispatch(input.data, input.reservoir)
            },
        )
    }
}

impl<P: PolicyCheck> ZoneTip403ProxyRegistry<P> {
    /// Dispatch based on the 4-byte selector.
    fn dispatch(&self, data: &[u8], reservoir: u64) -> PrecompileResult {
        let call = match ITIP403Registry::ITIP403RegistryCalls::abi_decode(data) {
            Ok(call) => call,
            Err(_) => return Ok(PrecompileOutput::revert(0, Bytes::new(), reservoir)),
        };

        match call {
            ITIP403Registry::ITIP403RegistryCalls::policyIdCounter(_) => {
                self.handle_policy_id_counter(reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::policyExists(call) => {
                self.handle_policy_exists(call.policyId, reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::policyData(call) => {
                self.handle_policy_data(call.policyId, reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::isAuthorized(call) => {
                self.handle_is_authorized(call.policyId, call.user, AuthRole::Transfer, reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::isAuthorizedSender(call) => {
                self.handle_is_authorized(call.policyId, call.user, AuthRole::Sender, reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::isAuthorizedRecipient(call) => {
                self.handle_is_authorized(call.policyId, call.user, AuthRole::Recipient, reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::isAuthorizedMintRecipient(call) => self
                .handle_is_authorized(call.policyId, call.user, AuthRole::MintRecipient, reservoir),
            ITIP403Registry::ITIP403RegistryCalls::compoundPolicyData(call) => {
                self.handle_compound_policy_data(call.policyId, reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::createPolicy(_) => {
                self.read_only_revert(reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::createPolicyWithAccounts(_) => {
                self.read_only_revert(reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::setPolicyAdmin(_) => {
                self.read_only_revert(reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::modifyPolicyWhitelist(_) => {
                self.read_only_revert(reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::modifyPolicyBlacklist(_) => {
                self.read_only_revert(reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::createCompoundPolicy(_) => {
                self.read_only_revert(reservoir)
            }
            ITIP403Registry::ITIP403RegistryCalls::receivePolicy(_)
            | ITIP403Registry::ITIP403RegistryCalls::validateReceivePolicy(_)
            | ITIP403Registry::ITIP403RegistryCalls::setReceivePolicy(_) => {
                Ok(PrecompileOutput::revert(0, Bytes::new(), reservoir))
            }
        }
    }

    fn read_only_revert(&self, reservoir: u64) -> PrecompileResult {
        debug!(target: "zone::precompile", "ZoneTip403ProxyRegistry: mutating call reverted");
        Ok(PrecompileOutput::revert(
            0,
            ReadOnlyRegistry {}.abi_encode().into(),
            reservoir,
        ))
    }

    /// Handle `isAuthorized(policyId, user)` and the directional variants.
    fn handle_is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
        reservoir: u64,
    ) -> PrecompileResult {
        let authorized = self.is_authorized(policy_id, user, role)?;
        let encoded = ITIP403Registry::isAuthorizedCall::abi_encode_returns(&authorized);
        Ok(PrecompileOutput::new(
            super::AUTH_CHECK_GAS,
            encoded.into(),
            reservoir,
        ))
    }

    /// Handle `policyData(policyId) -> (PolicyType, address admin)`.
    fn handle_policy_data(&self, policy_id: u64, reservoir: u64) -> PrecompileResult {
        // Builtins: reject-all is an empty whitelist, allow-all is an empty blacklist.
        let builtin_type = match policy_id {
            REJECT_ALL_POLICY_ID => Some(PolicyType::WHITELIST),
            ALLOW_ALL_POLICY_ID => Some(PolicyType::BLACKLIST),
            _ => None,
        };
        if let Some(policy_type) = builtin_type {
            let ret = ITIP403Registry::policyDataReturn {
                policyType: policy_type,
                admin: Address::ZERO,
            };
            let encoded = ITIP403Registry::policyDataCall::abi_encode_returns(&ret);
            return Ok(PrecompileOutput::new(
                POLICY_DATA_GAS,
                encoded.into(),
                reservoir,
            ));
        }

        let policy_type = self.provider.policy_type_sync(policy_id)?;

        let ret = ITIP403Registry::policyDataReturn {
            policyType: policy_type,
            admin: Address::ZERO,
        };
        let encoded = ITIP403Registry::policyDataCall::abi_encode_returns(&ret);
        Ok(PrecompileOutput::new(
            POLICY_DATA_GAS,
            encoded.into(),
            reservoir,
        ))
    }

    /// Handle `compoundPolicyData(policyId) -> (uint64, uint64, uint64)`.
    fn handle_compound_policy_data(&self, policy_id: u64, reservoir: u64) -> PrecompileResult {
        let (sender, recipient, mint_recipient) = self.provider.compound_policy_data(policy_id)?;

        let ret = ITIP403Registry::compoundPolicyDataReturn {
            senderPolicyId: sender,
            recipientPolicyId: recipient,
            mintRecipientPolicyId: mint_recipient,
        };
        let encoded = ITIP403Registry::compoundPolicyDataCall::abi_encode_returns(&ret);
        Ok(PrecompileOutput::new(
            POLICY_DATA_GAS,
            encoded.into(),
            reservoir,
        ))
    }

    /// Handle `policyExists(policyId) -> bool`.
    fn handle_policy_exists(&self, policy_id: u64, reservoir: u64) -> PrecompileResult {
        if matches!(policy_id, REJECT_ALL_POLICY_ID | ALLOW_ALL_POLICY_ID) {
            let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&true);
            return Ok(PrecompileOutput::new(
                POLICY_DATA_GAS,
                encoded.into(),
                reservoir,
            ));
        }

        let exists = self.provider.policy_exists(policy_id)?;
        let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&exists);
        Ok(PrecompileOutput::new(
            POLICY_DATA_GAS,
            encoded.into(),
            reservoir,
        ))
    }

    /// Handle `policyIdCounter() -> uint64`.
    fn handle_policy_id_counter(&self, reservoir: u64) -> PrecompileResult {
        let counter = self.provider.policy_id_counter();
        let encoded = ITIP403Registry::policyIdCounterCall::abi_encode_returns(&counter);
        Ok(PrecompileOutput::new(
            POLICY_DATA_GAS,
            encoded.into(),
            reservoir,
        ))
    }
}
