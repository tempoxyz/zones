//! Zone fee-manager precompile and protocol fee logic.
//!
//! Zones accept any fee token that is enabled on their L1 `ZonePortal`.
//! Fees are collected and credited directly in the user's fee token; the
//! Tempo FeeAMM routing path is intentionally disabled.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::{SolError, SolValue};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_contracts::precompiles::{FeeManagerError, IFeeManager, ITIPFeeAMM};
use tempo_precompiles::{
    DelegateCallNotAllowed, Precompile as TempoPrecompile, charge_input_cost, dispatch,
    mutate_void,
    storage::{Handler, StorageCtx, evm::EvmPrecompileStorageProvider},
    tip20::{TIP20Token, validate_usd_currency},
    tip20_factory::TIP20Factory,
    view,
};
use zone_primitives::constants::PORTAL_TOKEN_CONFIGS_SLOT;

use crate::{L1StorageReader, TempoState};

alloy_sol_types::sol! {
    error FeeAmmDisabled();
}

/// L1 portal storage access needed by zone fee-token validation.
pub trait ZonePortalReader: L1StorageReader {
    /// Address of the Tempo L1 `ZonePortal` backing this zone.
    fn portal_address(&self) -> Address;
}

/// Compute `ZonePortal._tokenConfigs[token]` storage slot.
pub fn portal_token_config_slot(token: Address) -> B256 {
    keccak256((token, PORTAL_TOKEN_CONFIGS_SLOT).abi_encode())
}

/// Zone fee manager.
///
/// This uses the upstream [`tempo_precompiles::tip_fee_manager::TipFeeManager`]
/// storage layout for user preferences and collected-fee ledgers so external
/// `IFeeManager` reads and `distributeFees` stay ABI-compatible.
#[derive(Debug, Clone)]
pub struct ZoneFeeManager<P> {
    provider: P,
}

impl<P: ZonePortalReader> ZoneFeeManager<P> {
    /// Create a new zone fee manager backed by an L1 portal reader.
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Returns true if `token` is enabled on the L1 portal at the zone's
    /// current Tempo checkpoint.
    pub fn is_token_enabled_current(&self, token: Address) -> tempo_precompiles::Result<bool> {
        let block_number = TempoState::new().current_tempo_block_number()?;
        self.is_token_enabled_at(token, block_number)
    }

    /// Require that `token` is enabled on the L1 portal at the zone's current
    /// Tempo checkpoint.
    pub fn ensure_token_enabled_current(&self, token: Address) -> tempo_precompiles::Result<()> {
        if self.is_token_enabled_current(token)? {
            Ok(())
        } else {
            Err(FeeManagerError::invalid_token().into())
        }
    }

    fn is_token_enabled_at(
        &self,
        token: Address,
        block_number: u64,
    ) -> tempo_precompiles::Result<bool> {
        if self.provider.portal_address().is_zero() {
            return Ok(true);
        }

        let slot = portal_token_config_slot(token);
        let value = self
            .provider
            .read_l1_storage(self.provider.portal_address(), slot, block_number)
            .map_err(|err| {
                tempo_precompiles::error::TempoPrecompileError::Fatal(err.to_string())
            })?;

        // TokenConfig.enabled is the lowest byte of the packed struct.
        Ok(value.as_slice()[31] & 1 != 0)
    }

    fn validate_fee_token(&self, token: Address) -> tempo_precompiles::Result<()> {
        if !TIP20Factory::new().is_tip20(token)? {
            return Err(FeeManagerError::invalid_token().into());
        }

        validate_usd_currency(token)?;
        self.ensure_token_enabled_current(token)
    }

    /// Set caller's user fee-token preference after zone enabled-token
    /// validation.
    pub fn set_user_token(
        &self,
        sender: Address,
        call: IFeeManager::setUserTokenCall,
    ) -> tempo_precompiles::Result<()> {
        self.validate_fee_token(call.token)?;
        tempo_precompiles::tip_fee_manager::TipFeeManager::new().set_user_token(sender, call)
    }

    /// Preserve the upstream validator-token preference API for compatibility.
    ///
    /// Zone protocol fee collection ignores validator preferences and credits
    /// fees directly in the user's token.
    pub fn set_validator_token(
        &self,
        sender: Address,
        call: IFeeManager::setValidatorTokenCall,
    ) -> tempo_precompiles::Result<()> {
        self.validate_fee_token(call.token)?;
        let beneficiary = StorageCtx.beneficiary();
        tempo_precompiles::tip_fee_manager::TipFeeManager::new().set_validator_token(
            sender,
            call,
            beneficiary,
        )
    }

    /// Collect the maximum possible fee before transaction execution.
    ///
    /// Unlike the Tempo L1 fee manager, this never checks or reserves FeeAMM
    /// liquidity because zones settle in the user's fee token directly.
    pub fn collect_fee_pre_tx(
        &self,
        fee_payer: Address,
        user_token: Address,
        max_amount: U256,
        _beneficiary: Address,
        _skip_liquidity_check: bool,
    ) -> tempo_precompiles::Result<Address> {
        self.validate_fee_token(user_token)?;

        let mut token = TIP20Token::from_address(user_token)?;
        token.ensure_transfer_authorized(
            fee_payer,
            tempo_precompiles::tip_fee_manager::TIP_FEE_MANAGER_ADDRESS,
        )?;
        token.transfer_fee_pre_tx(fee_payer, max_amount)?;

        Ok(user_token)
    }

    /// Settle the actual fee after transaction execution.
    ///
    /// Refunds unused fee tokens and credits the validator in the same token.
    pub fn collect_fee_post_tx(
        &self,
        fee_payer: Address,
        actual_spending: U256,
        refund_amount: U256,
        fee_token: Address,
        beneficiary: Address,
    ) -> tempo_precompiles::Result<U256> {
        self.validate_fee_token(fee_token)?;

        let mut token = TIP20Token::from_address(fee_token)?;
        token.transfer_fee_post_tx(fee_payer, refund_amount, actual_spending)?;

        if !actual_spending.is_zero() {
            tempo_precompiles::tip_fee_manager::TipFeeManager::new().collected_fees[beneficiary]
                [fee_token]
                .sinc(actual_spending)?;
        }

        Ok(actual_spending)
    }

    fn fee_amm_disabled(&self) -> PrecompileResult {
        Ok(StorageCtx.revert_output(FeeAmmDisabled {}.abi_encode().into()))
    }

    /// Create a [`DynPrecompile`] for the zone fee-manager ABI.
    pub fn create(
        provider: P,
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> DynPrecompile
    where
        P: Clone + Send + Sync + 'static,
    {
        let manager = Self::new(provider);
        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();

        DynPrecompile::new_stateful(
            PrecompileId::Custom("ZoneFeeManager".into()),
            move |input| {
                if !input.is_direct_call() {
                    return Ok(PrecompileOutput::revert(
                        0,
                        DelegateCallNotAllowed {}.abi_encode().into(),
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
                    let mut manager = manager.clone();
                    manager.call(input.data, input.caller)
                })
            },
        )
    }
}

impl<P: ZonePortalReader> TempoPrecompile for ZoneFeeManager<P> {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut StorageCtx::default(), calldata) {
            return err;
        }

        dispatch!(
            calldata,
            |call| match call {
                IFeeManager::IFeeManagerCalls {
                    userTokens(call) => view(call, |c| {
                        tempo_precompiles::tip_fee_manager::TipFeeManager::new().user_tokens(c)
                    }),
                    validatorTokens(call) => view(call, |c| {
                        tempo_precompiles::tip_fee_manager::TipFeeManager::new()
                            .get_validator_token(c.validator)
                    }),
                    collectedFees(call) => view(call, |c| {
                        tempo_precompiles::tip_fee_manager::TipFeeManager::new().collected_fees
                            [c.validator][c.token]
                            .read()
                    }),
                    setValidatorToken(call) => {
                        mutate_void(call, msg_sender, |s, c| self.set_validator_token(s, c))
                    },
                    setUserToken(call) => {
                        mutate_void(call, msg_sender, |s, c| self.set_user_token(s, c))
                    },
                    distributeFees(call) => mutate_void(call, msg_sender, |_, c| {
                        tempo_precompiles::tip_fee_manager::TipFeeManager::new()
                            .distribute_fees(c.validator, c.token)
                    }),
                }
                ITIPFeeAMM::ITIPFeeAMMCalls {
                    M(_) => self.fee_amm_disabled(),
                    N(_) => self.fee_amm_disabled(),
                    SCALE(_) => self.fee_amm_disabled(),
                    MIN_LIQUIDITY(_) => self.fee_amm_disabled(),
                    getPoolId(_) => self.fee_amm_disabled(),
                    getPool(_) => self.fee_amm_disabled(),
                    pools(_) => self.fee_amm_disabled(),
                    totalSupply(_) => self.fee_amm_disabled(),
                    liquidityBalances(_) => self.fee_amm_disabled(),
                    mint(_) => self.fee_amm_disabled(),
                    burn(_) => self.fee_amm_disabled(),
                    rebalanceSwap(_) => self.fee_amm_disabled(),
                }
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::address;
    use alloy_rlp::Encodable as _;
    use revm::precompile::PrecompileError;
    use tempo_precompiles::{
        TIP_FEE_MANAGER_ADDRESS,
        storage::{ContractStorage, StorageCtx, hashmap::HashMapStorageProvider},
        test_util::TIP20Setup,
        tip20::ITIP20,
    };
    use tempo_primitives::TempoHeader;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[derive(Debug, Clone)]
    struct MockPortalReader {
        portal: Address,
        enabled: bool,
    }

    impl L1StorageReader for MockPortalReader {
        fn read_l1_storage(
            &self,
            account: Address,
            slot: B256,
            _block_number: u64,
        ) -> Result<B256, PrecompileError> {
            assert_eq!(account, self.portal);
            assert_ne!(slot, B256::ZERO);
            let mut bytes = [0u8; 32];
            bytes[31] = u8::from(self.enabled);
            Ok(B256::new(bytes))
        }
    }

    impl ZonePortalReader for MockPortalReader {
        fn portal_address(&self) -> Address {
            self.portal
        }
    }

    fn initialize_tempo_state() -> tempo_precompiles::Result<()> {
        let mut header = Vec::new();
        TempoHeader::default().encode(&mut header);
        TempoState::new().initialize(&header)
    }

    #[test]
    fn direct_fee_collection_credits_fee_token_without_amm() -> TestResult {
        let mut storage = HashMapStorageProvider::new(1);
        let admin = address!("0x0000000000000000000000000000000000000a11");
        let user = address!("0x0000000000000000000000000000000000000b0b");
        let validator = address!("0x0000000000000000000000000000000000000c0c");
        let portal = address!("0x0000000000000000000000000000000000000d0d");

        StorageCtx::enter(&mut storage, || {
            initialize_tempo_state()?;
            let token = TIP20Setup::create("Zone USD", "zUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000u64))
                .with_approval(user, TIP_FEE_MANAGER_ADDRESS, U256::MAX)
                .apply()?;

            let manager = ZoneFeeManager::new(MockPortalReader {
                portal,
                enabled: true,
            });

            manager.collect_fee_pre_tx(
                user,
                token.address(),
                U256::from(5_000u64),
                validator,
                false,
            )?;
            let credited = manager.collect_fee_post_tx(
                user,
                U256::from(3_000u64),
                U256::from(2_000u64),
                token.address(),
                validator,
            )?;

            assert_eq!(credited, U256::from(3_000u64));
            assert_eq!(
                tempo_precompiles::tip_fee_manager::TipFeeManager::new().collected_fees[validator]
                    [token.address()]
                .read()?,
                U256::from(3_000u64)
            );
            assert_eq!(
                token.balance_of(ITIP20::balanceOfCall { account: user })?,
                U256::from(7_000u64)
            );
            Ok::<_, tempo_precompiles::error::TempoPrecompileError>(())
        })?;

        Ok(())
    }

    #[test]
    fn disabled_portal_token_is_not_valid_fee_token() -> TestResult {
        let mut storage = HashMapStorageProvider::new(1);
        let admin = address!("0x0000000000000000000000000000000000000a11");
        let user = address!("0x0000000000000000000000000000000000000b0b");
        let portal = address!("0x0000000000000000000000000000000000000d0d");

        StorageCtx::enter(&mut storage, || {
            initialize_tempo_state()?;
            let token = TIP20Setup::create("Zone USD", "zUSD", admin).apply()?;
            let manager = ZoneFeeManager::new(MockPortalReader {
                portal,
                enabled: false,
            });

            let err = manager
                .set_user_token(
                    user,
                    IFeeManager::setUserTokenCall {
                        token: token.address(),
                    },
                )
                .expect_err("disabled token should be rejected");
            assert!(matches!(
                err,
                tempo_precompiles::error::TempoPrecompileError::FeeManagerError(_)
            ));
            Ok::<_, tempo_precompiles::error::TempoPrecompileError>(())
        })?;

        Ok(())
    }
}
