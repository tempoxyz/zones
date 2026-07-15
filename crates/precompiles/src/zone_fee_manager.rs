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
use tempo_precompiles::{
    DelegateCallNotAllowed, charge_input_cost, dispatch,
    error::{Result, TempoPrecompileError},
    mutate_void,
    storage::{Handler, Mapping, StorageCtx, evm::EvmPrecompileStorageProvider},
    tip20::{ITIP20, TIP20Token, validate_usd_currency},
    view,
};
use tempo_precompiles_macros::contract;
use tempo_zone_contracts::{IZoneFeeManager, PORTAL_TOKEN_CONFIGS_SLOT, ZONE_FEE_MANAGER_ADDRESS};

use crate::{L1StorageReader, TempoState};

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
/// This layout is owned by the zone implementation. In particular, no Tempo
/// `TipFeeManager` storage slots are read or overwritten.
#[contract(addr = ZONE_FEE_MANAGER_ADDRESS)]
pub struct ZoneFeeManager {
    user_tokens: Mapping<Address, Address>,
    collected_fees: Mapping<Address, Mapping<Address, U256>>,
}

impl ZoneFeeManager {
    /// Initializes the precompile account marker in genesis.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    fn map_reader_error(error: PrecompileError) -> TempoPrecompileError {
        TempoPrecompileError::Fatal(error.to_string())
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
    pub fn collect_fee_pre_tx<P: ZoneConfigReader>(
        &mut self,
        provider: &P,
        fee_payer: Address,
        fee_token: Address,
        max_amount: U256,
    ) -> Result<Address> {
        self.ensure_enabled(provider, fee_token)?;

        let mut token = TIP20Token::from_address(fee_token)?;
        // Tempo's specialized fee-transfer helpers use the canonical fee-manager
        // address as protocol custody. The public precompile and all accounting at
        // that address remain disabled on zones.
        token.ensure_transfer_authorized(fee_payer, tempo_precompiles::TIP_FEE_MANAGER_ADDRESS)?;
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
    pub fn distribute_fees(&mut self, sequencer: Address, token: Address) -> Result<()> {
        StorageCtx.set_tip1060_storage_credit_minting(false);

        let amount = self.collected_fees[sequencer][token].read()?;
        if amount.is_zero() {
            return Ok(());
        }
        self.collected_fees[sequencer][token].write(U256::ZERO)?;

        let mut tip20 = TIP20Token::from_address(token)?;
        // `transfer_fee_pre_tx` escrows here inside TIP-20 storage; ZoneFeeManager
        // owns the corresponding sequencer ledger and is the only enabled fee API.
        tip20.transfer(
            tempo_precompiles::TIP_FEE_MANAGER_ADDRESS,
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

    /// Wraps the public ZoneFeeManager ABI for EVM registration.
    pub fn create<P: ZoneConfigReader>(
        provider: P,
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
                    Self::new().call_with_provider(&provider, input.data, input.caller)
                })
            },
        )
    }

    fn call_with_provider<P: ZoneConfigReader>(
        &mut self,
        provider: &P,
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
                    self.distribute_fees(call.sequencer, call.token)
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
            };
            let mut manager = ZoneFeeManager::new();

            for (token, max, used) in [
                (alpha.address(), U256::from(2_000), U256::from(1_250)),
                (beta.address(), U256::from(3_000), U256::from(2_500)),
            ] {
                manager.collect_fee_pre_tx(&provider, user, token, max)?;
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

                manager.distribute_fees(sequencer, token)?;
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
            };

            let error = ZoneFeeManager::new()
                .collect_fee_pre_tx(&provider, user, token.address(), U256::from(1_000))
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
}
