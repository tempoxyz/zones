//! Zone-specific protocol fee manager.
//!
//! Zone fees never use Tempo's FeeAMM. The transaction's resolved fee token is
//! checked against the portal registry at the finalized [`TempoState`] checkpoint,
//! escrowed for execution, and credited to the sequencer in that same token.

use alloc::string::ToString;
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, U256, keccak256};
use alloy_sol_types::{SolError, SolValue};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_contracts::precompiles::TIP20Error;
use tempo_precompiles::{
    DelegateCallNotAllowed, charge_input_cost, dispatch,
    error::{Result, TempoPrecompileError},
    mutate_void,
    storage::{ContractStorage, Handler, Mapping, StorageCtx, evm::EvmPrecompileStorageProvider},
    tip20::{ITIP20, TIP20Token, validate_usd_currency},
    tip403_registry::AuthRole as TempoAuthRole,
    view,
};
use tempo_precompiles_macros::contract;
use tempo_zone_contracts::{IZoneFeeManager, PORTAL_TOKEN_CONFIGS_SLOT, ZONE_FEE_MANAGER_ADDRESS};
use zone_primitives::policy::AuthRole;

use crate::{
    L1StorageReader, TempoState, policy::PolicyCheck, tip403_proxy::ZoneTip403ProxyRegistry,
};

/// L1 state access required to resolve [`ZoneConfig`](https://github.com/tempoxyz/tempo-zones)
/// token enablement at the zone's finalized Tempo checkpoint.
pub trait ZoneConfigReader: L1StorageReader {
    /// Address of the ZonePortal whose registry backs ZoneConfig.
    fn zone_portal_address(&self) -> Address;

    /// Apply the same `_tokenConfigs[token].enabled` lookup as `ZoneConfig.isEnabledToken`.
    fn is_enabled_token(
        &self,
        token: Address,
        block_number: u64,
    ) -> core::result::Result<bool, PrecompileError> {
        let slot = keccak256((token, PORTAL_TOKEN_CONFIGS_SLOT).abi_encode());
        let value = self.read_l1_storage(self.zone_portal_address(), slot, block_number)?;
        Ok(value[31] != 0)
    }
}

/// Zone fee manager storage.
///
/// The first three slots retain Tempo's fee-manager shape so shared tooling can
/// resolve user preferences without maintaining a second storage decoder. Zones
/// do not use validator preferences, so slot zero remains empty.
#[contract(addr = ZONE_FEE_MANAGER_ADDRESS)]
pub struct ZoneFeeManager {
    _validator_tokens: Mapping<Address, Address>,
    user_tokens: Mapping<Address, Address>,
    collected_fees: Mapping<Address, Mapping<Address, U256>>,
}

impl ZoneFeeManager {
    /// Initializes the precompile account marker in genesis.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    fn map_reader_error(error: PrecompileError) -> TempoPrecompileError {
        match error {
            PrecompileError::Fatal(message) => TempoPrecompileError::Fatal(message),
            error => TempoPrecompileError::Fatal(error.to_string()),
        }
    }

    fn is_enabled<P: ZoneConfigReader>(&self, provider: &P, token: Address) -> Result<bool> {
        let block_number = TempoState::new().finalized_block_number()?;
        provider
            .is_enabled_token(token, block_number)
            .map_err(Self::map_reader_error)
    }

    fn ensure_enabled<P: ZoneConfigReader>(&self, provider: &P, token: Address) -> Result<()> {
        if !self.is_enabled(provider, token)? {
            return Err(
                tempo_precompiles::tip_fee_manager::FeeManagerError::invalid_token().into(),
            );
        }
        Ok(())
    }

    /// Returns the fee token preference stored for `user`.
    pub fn user_token(&self, user: Address) -> Result<Address> {
        self.user_tokens[user].read()
    }

    /// Returns fees accrued to `sequencer` in `token`.
    pub fn collected_fees(&self, sequencer: Address, token: Address) -> Result<U256> {
        self.collected_fees[sequencer][token].read()
    }

    /// Stores a user's zone fee token preference after registry and currency validation.
    pub fn set_user_token<P: ZoneConfigReader>(
        &mut self,
        provider: &P,
        sender: Address,
        token: Address,
    ) -> Result<()> {
        self.ensure_enabled(provider, token)?;
        validate_usd_currency(token)?;

        if self.user_tokens[sender].read()? == token {
            return Ok(());
        }
        self.user_tokens[sender].write(token)?;
        self.emit_event(IZoneFeeManager::UserTokenSet {
            user: sender,
            token,
        })
    }

    /// Collects the maximum fee before execution without consulting FeeAMM state.
    pub fn collect_fee_pre_tx<P: ZoneConfigReader, R: PolicyCheck>(
        &mut self,
        provider: &P,
        registry: Option<&ZoneTip403ProxyRegistry<R>>,
        fee_payer: Address,
        fee_token: Address,
        max_amount: U256,
    ) -> Result<Address> {
        self.ensure_enabled(provider, fee_token)?;

        let mut token = TIP20Token::from_address(fee_token)?;
        self.ensure_fee_transfer_authorized(registry, &token, fee_payer)?;
        token.transfer_fee_pre_tx(fee_payer, max_amount)?;
        Ok(fee_token)
    }

    /// Refunds unused gas and credits the sequencer in the user's fee token.
    pub fn collect_fee_post_tx(
        &mut self,
        fee_payer: Address,
        actual_spending: U256,
        refund_amount: U256,
        fee_token: Address,
        sequencer: Address,
    ) -> Result<U256> {
        let mut token = TIP20Token::from_address(fee_token)?;
        token.transfer_fee_post_tx(fee_payer, refund_amount, actual_spending)?;

        if !actual_spending.is_zero() {
            self.collected_fees[sequencer][fee_token].sinc(actual_spending)?;
        }
        Ok(actual_spending)
    }

    /// Transfers a sequencer's accrued fees out of protocol custody.
    pub fn distribute_fees<R: PolicyCheck>(
        &mut self,
        registry: Option<&ZoneTip403ProxyRegistry<R>>,
        sequencer: Address,
        token: Address,
    ) -> Result<()> {
        StorageCtx.set_tip1060_storage_credit_minting(false);

        let amount = self.collected_fees[sequencer][token].read()?;
        if amount.is_zero() {
            return Ok(());
        }
        self.collected_fees[sequencer][token].write(U256::ZERO)?;

        let mut tip20 = TIP20Token::from_address(token)?;
        self.ensure_transfer_authorized(registry, token, self.address, sequencer)?;
        tip20.transfer(
            self.address,
            ITIP20::transferCall {
                to: sequencer,
                amount,
            },
        )?;
        self.emit_event(IZoneFeeManager::FeesDistributed {
            sequencer,
            token,
            amount,
        })
    }

    fn ensure_fee_transfer_authorized<R: PolicyCheck>(
        &self,
        registry: Option<&ZoneTip403ProxyRegistry<R>>,
        token: &TIP20Token,
        fee_payer: Address,
    ) -> Result<()> {
        let Some(registry) = registry else {
            return if self.storage.spec().is_t8() {
                token.ensure_authorized_as(&[(fee_payer, TempoAuthRole::sender())])
            } else {
                token.ensure_transfer_authorized(fee_payer, self.address)
            };
        };

        let policy_id = registry
            .resolve_transfer_policy_id(token.address())
            .map_err(Self::map_reader_error)?;
        let authorized = if self.storage.spec().is_t8() {
            registry
                .is_authorized(policy_id, fee_payer, AuthRole::Sender)
                .map_err(Self::map_reader_error)?
        } else {
            registry
                .is_transfer_authorized(policy_id, fee_payer, self.address)
                .map_err(Self::map_reader_error)?
        };
        if !authorized {
            return Err(TIP20Error::policy_forbids().into());
        }
        Ok(())
    }

    fn ensure_transfer_authorized<R: PolicyCheck>(
        &self,
        registry: Option<&ZoneTip403ProxyRegistry<R>>,
        token: Address,
        from: Address,
        to: Address,
    ) -> Result<()> {
        let Some(registry) = registry else {
            return TIP20Token::from_address(token)?.ensure_transfer_authorized(from, to);
        };
        let policy_id = registry
            .resolve_transfer_policy_id(token)
            .map_err(Self::map_reader_error)?;
        if !registry
            .is_transfer_authorized(policy_id, from, to)
            .map_err(Self::map_reader_error)?
        {
            return Err(TIP20Error::policy_forbids().into());
        }
        Ok(())
    }

    /// Wraps the public ZoneFeeManager ABI for EVM registration.
    pub fn create<P: ZoneConfigReader, R: PolicyCheck + Clone + Send + Sync + 'static>(
        provider: P,
        registry: Option<ZoneTip403ProxyRegistry<R>>,
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> DynPrecompile {
        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();

        DynPrecompile::new_stateful(
            PrecompileId::Custom("ZoneFeeManager".into()),
            move |input| {
                if !input.is_direct_call() {
                    return Ok(PrecompileOutput::revert(
                        0,
                        SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
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
                    Self::new().call_with_provider(
                        &provider,
                        registry.as_ref(),
                        input.data,
                        input.caller,
                    )
                })
            },
        )
    }

    fn call_with_provider<P: ZoneConfigReader, R: PolicyCheck>(
        &mut self,
        provider: &P,
        registry: Option<&ZoneTip403ProxyRegistry<R>>,
        calldata: &[u8],
        msg_sender: Address,
    ) -> PrecompileResult {
        if let Some(error) = charge_input_cost(&mut self.storage, calldata) {
            return error;
        }

        dispatch!(calldata, |call| match call {
            IZoneFeeManager::IZoneFeeManagerCalls {
                userTokens(call) => view(call, |call| self.user_token(call.user)),
                collectedFees(call) => view(call, |call| {
                    self.collected_fees(call.sequencer, call.token)
                }),
                isEnabledToken(call) => view(call, |call| self.is_enabled(provider, call.token)),
                setUserToken(call) => mutate_void(call, msg_sender, |sender, call| {
                    self.set_user_token(provider, sender, call.token)
                }),
                distributeFees(call) => mutate_void(call, msg_sender, |_, call| {
                    self.distribute_fees(registry, call.sequencer, call.token)
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::{B256, address};
    use tempo_precompiles::{
        TIP_FEE_MANAGER_ADDRESS,
        storage::{ContractStorage, StorageCtx, hashmap::HashMapStorageProvider},
        test_util::TIP20Setup,
        tip_fee_manager::{TipFeeManager, amm::PoolKey},
        tip20::{ITIP20, TIP20Token},
    };

    use super::*;

    type TestResult = core::result::Result<(), Box<dyn std::error::Error>>;

    #[derive(Clone)]
    struct MockZoneConfig {
        portal: Address,
        enabled: Vec<Address>,
        authorized: bool,
    }

    impl L1StorageReader for MockZoneConfig {
        fn read_l1_storage(
            &self,
            account: Address,
            slot: B256,
            block_number: u64,
        ) -> core::result::Result<B256, PrecompileError> {
            assert_eq!(account, self.portal);
            assert_eq!(block_number, 0);

            let enabled = self
                .enabled
                .iter()
                .any(|token| keccak256((*token, PORTAL_TOKEN_CONFIGS_SLOT).abi_encode()) == slot);
            Ok(B256::from(U256::from(enabled as u8).to_be_bytes()))
        }
    }

    impl ZoneConfigReader for MockZoneConfig {
        fn zone_portal_address(&self) -> Address {
            self.portal
        }
    }

    impl PolicyCheck for MockZoneConfig {
        fn is_authorized(
            &self,
            _policy_id: u64,
            _user: Address,
            _role: AuthRole,
        ) -> core::result::Result<bool, PrecompileError> {
            Ok(self.authorized)
        }

        fn resolve_transfer_policy_id(
            &self,
            _token: Address,
        ) -> core::result::Result<u64, PrecompileError> {
            Ok(1)
        }

        fn policy_type_sync(
            &self,
            _policy_id: u64,
        ) -> core::result::Result<
            tempo_contracts::precompiles::ITIP403Registry::PolicyType,
            PrecompileError,
        > {
            unreachable!("not used by fee-manager tests")
        }

        fn compound_policy_data(
            &self,
            _policy_id: u64,
        ) -> core::result::Result<(u64, u64, u64), PrecompileError> {
            unreachable!("not used by fee-manager tests")
        }

        fn policy_exists(&self, _policy_id: u64) -> core::result::Result<bool, PrecompileError> {
            unreachable!("not used by fee-manager tests")
        }

        fn policy_id_counter(&self) -> u64 {
            1
        }
    }

    #[test]
    fn collects_enabled_tokens_without_touching_fee_amm() -> TestResult {
        let mut storage = HashMapStorageProvider::new(1);
        let admin = Address::random();
        let user = Address::random();
        let sequencer = Address::random();

        StorageCtx::enter(&mut storage, || {
            let alpha = TIP20Setup::create("Alpha USD", "aUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000u64))
                .apply()?;
            let beta = TIP20Setup::create("Beta USD", "bUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000u64))
                .apply()?;
            let provider = MockZoneConfig {
                portal: address!("0x0000000000000000000000000000000000001234"),
                enabled: vec![alpha.address(), beta.address()],
                authorized: true,
            };
            let registry = ZoneTip403ProxyRegistry::new(provider.clone());
            let mut manager = ZoneFeeManager::new();

            for (token, max, used) in [
                (alpha.address(), U256::from(2_000), U256::from(1_250)),
                (beta.address(), U256::from(3_000), U256::from(2_500)),
            ] {
                manager.collect_fee_pre_tx(&provider, Some(&registry), user, token, max)?;
                manager.collect_fee_post_tx(user, used, max - used, token, sequencer)?;

                assert_eq!(manager.collected_fees(sequencer, token)?, used);
                assert_eq!(
                    TIP20Token::from_address(token)?
                        .balance_of(ITIP20::balanceOfCall { account: user })?,
                    U256::from(10_000) - used
                );
                assert_eq!(
                    TIP20Token::from_address(token)?.balance_of(ITIP20::balanceOfCall {
                        account: TIP_FEE_MANAGER_ADDRESS,
                    })?,
                    used
                );

                manager.distribute_fees(Some(&registry), sequencer, token)?;
                assert_eq!(manager.collected_fees(sequencer, token)?, U256::ZERO);
                assert_eq!(
                    TIP20Token::from_address(token)?
                        .balance_of(ITIP20::balanceOfCall { account: sequencer })?,
                    used
                );
            }

            let pool = TipFeeManager::new().pools
                [PoolKey::new(alpha.address(), beta.address()).get_id()]
            .read()?;
            assert_eq!(pool.reserve_user_token, 0);
            assert_eq!(pool.reserve_validator_token, 0);
            Ok(())
        })
    }

    #[test]
    fn rejects_tokens_disabled_in_zone_config() -> TestResult {
        let mut storage = HashMapStorageProvider::new(1);
        let admin = Address::random();
        let user = Address::random();

        StorageCtx::enter(&mut storage, || {
            let token = TIP20Setup::create("Disabled USD", "dUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000u64))
                .apply()?;
            let provider = MockZoneConfig {
                portal: Address::random(),
                enabled: Vec::new(),
                authorized: true,
            };
            let registry = ZoneTip403ProxyRegistry::new(provider.clone());

            let error = ZoneFeeManager::new()
                .collect_fee_pre_tx(
                    &provider,
                    Some(&registry),
                    user,
                    token.address(),
                    U256::from(1_000),
                )
                .unwrap_err();
            assert!(matches!(error, TempoPrecompileError::FeeManagerError(_)));
            assert_eq!(
                TIP20Token::from_address(token.address())?
                    .balance_of(ITIP20::balanceOfCall { account: user })?,
                U256::from(10_000)
            );
            Ok(())
        })
    }

    #[test]
    fn rejects_fee_collection_forbidden_by_l1_policy() -> TestResult {
        let mut storage = HashMapStorageProvider::new(1);
        let admin = Address::random();
        let user = Address::random();

        StorageCtx::enter(&mut storage, || {
            let token = TIP20Setup::create("Restricted USD", "rUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000u64))
                .apply()?;
            let provider = MockZoneConfig {
                portal: Address::random(),
                enabled: vec![token.address()],
                authorized: false,
            };
            let registry = ZoneTip403ProxyRegistry::new(provider.clone());

            let error = ZoneFeeManager::new()
                .collect_fee_pre_tx(
                    &provider,
                    Some(&registry),
                    user,
                    token.address(),
                    U256::from(1_000),
                )
                .unwrap_err();

            assert_eq!(error, TIP20Error::policy_forbids().into());
            Ok(())
        })
    }

    #[test]
    fn preserves_zone_rpc_error_marker() {
        let error = ZoneFeeManager::map_reader_error(crate::zone_rpc_error("unavailable"));
        assert_eq!(
            error,
            TempoPrecompileError::Fatal("[zone rpc] unavailable".into())
        );
    }
}
