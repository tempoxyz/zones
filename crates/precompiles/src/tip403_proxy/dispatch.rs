//! ABI dispatch for the [`ZoneTip403ProxyRegistry`] precompile.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_contracts::precompiles::ITIP403Registry::{self, PolicyType};
use tempo_precompiles::{
    Precompile as TempoPrecompile, charge_input_cost, dispatch,
    storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
    tip403_registry::{ALLOW_ALL_POLICY_ID, REJECT_ALL_POLICY_ID},
};
use tracing::{debug, warn};
use zone_primitives::policy::AuthRole;

use super::{POLICY_DATA_GAS, ReadOnlyRegistry, ZoneTip403ProxyRegistry};
use crate::policy::PolicyCheck;

impl<P: PolicyCheck + Clone + Send + Sync + 'static> ZoneTip403ProxyRegistry<P> {
    /// Create a [`DynPrecompile`] that dispatches TIP-403 registry calls
    /// to the zone's policy provider.
    pub fn create(
        provider: P,
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> DynPrecompile {
        let registry = Self::new(provider);
        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();
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

                let mut storage = EvmPrecompileStorageProvider::new(
                    input.internals,
                    input.gas,
                    input.reservoir,
                    spec,
                    amsterdam_eip8037_enabled,
                    input.is_static,
                    gas_params.clone(),
                );

                StorageCtx::enter(&mut storage, || {
                    let mut registry = registry.clone();
                    registry.call(input.data, input.caller)
                })
            },
        )
    }
}

impl<P: PolicyCheck> TempoPrecompile for ZoneTip403ProxyRegistry<P> {
    /// Dispatch based on the 4-byte selector.
    fn call(&mut self, calldata: &[u8], _msg_sender: Address) -> PrecompileResult {
        let mut storage = StorageCtx::default();
        if let Some(err) = charge_input_cost(&mut storage, calldata) {
            return err;
        }

        dispatch!(
            calldata,
            |call| match call {
                ITIP403Registry::ITIP403RegistryCalls {
                    policyIdCounter(_) => self.handle_policy_id_counter(),
                    policyExists(call) => self.handle_policy_exists(call.policyId),
                    policyData(call) => self.handle_policy_data(call.policyId),
                    isAuthorized(call) => {
                        self.handle_is_authorized(call.policyId, call.user, AuthRole::Transfer)
                    },
                    isAuthorizedSender(call) => {
                        self.handle_is_authorized(call.policyId, call.user, AuthRole::Sender)
                    },
                    isAuthorizedRecipient(call) => {
                        self.handle_is_authorized(call.policyId, call.user, AuthRole::Recipient)
                    },
                    isAuthorizedMintRecipient(call) => {
                        self.handle_is_authorized(call.policyId, call.user, AuthRole::MintRecipient)
                    },
                    compoundPolicyData(call) => {
                        self.handle_compound_policy_data(call.policyId)
                    },
                    createPolicy(_) => self.read_only_revert(),
                    createPolicyWithAccounts(_) => self.read_only_revert(),
                    setPolicyAdmin(_) => self.read_only_revert(),
                    modifyPolicyWhitelist(_) => self.read_only_revert(),
                    modifyPolicyBlacklist(_) => self.read_only_revert(),
                    createCompoundPolicy(_) => self.read_only_revert(),
                    receivePolicy(_) => Ok(StorageCtx::default().revert_output(Bytes::new())),
                    validateReceivePolicy(_) => Ok(StorageCtx::default().revert_output(Bytes::new())),
                    setReceivePolicy(_) => Ok(StorageCtx::default().revert_output(Bytes::new())),
                }
            },
        )
    }
}

impl<P: PolicyCheck> ZoneTip403ProxyRegistry<P> {
    fn read_only_revert(&self) -> PrecompileResult {
        debug!(target: "zone::precompile", "ZoneTip403ProxyRegistry: mutating call reverted");
        Ok(StorageCtx::default().revert_output(ReadOnlyRegistry {}.abi_encode().into()))
    }

    /// Handle `isAuthorized(policyId, user)` and the directional variants.
    fn handle_is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> PrecompileResult {
        let authorized = self.is_authorized(policy_id, user, role)?;
        let mut storage = StorageCtx::default();
        if storage.deduct_gas(super::AUTH_CHECK_GAS).is_err() {
            return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
        }
        let encoded = ITIP403Registry::isAuthorizedCall::abi_encode_returns(&authorized);
        Ok(storage.success_output(encoded.into()))
    }

    /// Handle `policyData(policyId) -> (PolicyType, address admin)`.
    fn handle_policy_data(&self, policy_id: u64) -> PrecompileResult {
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
            let mut storage = StorageCtx::default();
            if storage.deduct_gas(POLICY_DATA_GAS).is_err() {
                return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
            }
            let encoded = ITIP403Registry::policyDataCall::abi_encode_returns(&ret);
            return Ok(storage.success_output(encoded.into()));
        }

        let policy_type = self.provider.policy_type_sync(policy_id)?;

        let ret = ITIP403Registry::policyDataReturn {
            policyType: policy_type,
            admin: Address::ZERO,
        };
        let mut storage = StorageCtx::default();
        if storage.deduct_gas(POLICY_DATA_GAS).is_err() {
            return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
        }
        let encoded = ITIP403Registry::policyDataCall::abi_encode_returns(&ret);
        Ok(storage.success_output(encoded.into()))
    }

    /// Handle `compoundPolicyData(policyId) -> (uint64, uint64, uint64)`.
    fn handle_compound_policy_data(&self, policy_id: u64) -> PrecompileResult {
        let (sender, recipient, mint_recipient) = self.provider.compound_policy_data(policy_id)?;

        let ret = ITIP403Registry::compoundPolicyDataReturn {
            senderPolicyId: sender,
            recipientPolicyId: recipient,
            mintRecipientPolicyId: mint_recipient,
        };
        let mut storage = StorageCtx::default();
        if storage.deduct_gas(POLICY_DATA_GAS).is_err() {
            return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
        }
        let encoded = ITIP403Registry::compoundPolicyDataCall::abi_encode_returns(&ret);
        Ok(storage.success_output(encoded.into()))
    }

    /// Handle `policyExists(policyId) -> bool`.
    fn handle_policy_exists(&self, policy_id: u64) -> PrecompileResult {
        if matches!(policy_id, REJECT_ALL_POLICY_ID | ALLOW_ALL_POLICY_ID) {
            let mut storage = StorageCtx::default();
            if storage.deduct_gas(POLICY_DATA_GAS).is_err() {
                return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
            }
            let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&true);
            return Ok(storage.success_output(encoded.into()));
        }

        let exists = self.provider.policy_exists(policy_id)?;
        let mut storage = StorageCtx::default();
        if storage.deduct_gas(POLICY_DATA_GAS).is_err() {
            return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
        }
        let encoded = ITIP403Registry::policyExistsCall::abi_encode_returns(&exists);
        Ok(storage.success_output(encoded.into()))
    }

    /// Handle `policyIdCounter() -> uint64`.
    fn handle_policy_id_counter(&self) -> PrecompileResult {
        let counter = self.provider.policy_id_counter();
        let mut storage = StorageCtx::default();
        if storage.deduct_gas(POLICY_DATA_GAS).is_err() {
            return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
        }
        let encoded = ITIP403Registry::policyIdCounterCall::abi_encode_returns(&counter);
        Ok(storage.success_output(encoded.into()))
    }
}
