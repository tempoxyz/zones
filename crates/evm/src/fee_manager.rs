//! Adapter between Tempo's protocol fee hooks and the zone fee-manager precompile.

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use revm::{Database, context::Journal};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{
    DEFAULT_FEE_TOKEN,
    error::Result,
    storage::{Handler, StorageActions},
};
use tempo_revm::{ProtocolFeeManager, TempoStateAccess, TempoTx, TempoTxEnv};
use tempo_zone_contracts::{IZoneFeeManager, ZONE_FEE_MANAGER_ADDRESS};
use zone_l1::state::L1StateProvider;
use zone_precompiles::ZoneFeeManager;

/// Zone implementation of Tempo's internal protocol fee hooks.
#[derive(Debug, Clone)]
pub(crate) struct ZoneProtocolFeeManager {
    provider: L1StateProvider,
}

impl ZoneProtocolFeeManager {
    pub(crate) const fn new(provider: L1StateProvider) -> Self {
        Self { provider }
    }
}

impl<DB: Database> ProtocolFeeManager<DB> for ZoneProtocolFeeManager {
    fn get_fee_token(
        &self,
        journal: &mut Journal<DB>,
        tx: &TempoTxEnv,
        fee_payer: Address,
        spec: TempoHardfork,
        actions: StorageActions,
    ) -> Result<Address> {
        if let Some(token) = tx.fee_token() {
            return Ok(token);
        }

        // A direct preference update pays in the newly selected token, matching
        // the behavior of Tempo's public fee manager.
        if !tx.is_aa()
            && fee_payer == tx.caller()
            && let Some((kind, input)) = tx.calls().next()
            && kind.to() == Some(&ZONE_FEE_MANAGER_ADDRESS)
            && let Ok(call) = IZoneFeeManager::setUserTokenCall::abi_decode(input)
        {
            return Ok(call.token);
        }

        let preferred = journal.with_read_only_storage_ctx(spec, actions.clone(), || {
            ZoneFeeManager::new().user_tokens[fee_payer].read()
        })?;
        if !preferred.is_zero() {
            return Ok(preferred);
        }

        // Preserve Tempo's TIP-20 call inference and default-token behavior for
        // users that have not selected a zone preference.
        journal.get_fee_token(tx, fee_payer, spec, actions)
    }

    fn get_validator_token(
        &self,
        _journal: &mut Journal<DB>,
        _beneficiary: Address,
        _spec: TempoHardfork,
        _actions: StorageActions,
    ) -> Result<Address> {
        // Zones never negotiate a validator token or enter an AMM route.
        Ok(DEFAULT_FEE_TOKEN)
    }

    fn collect_fee_pre_tx(
        &self,
        fee_payer: Address,
        user_token: Address,
        max_amount: U256,
        _beneficiary: Address,
        _skip_liquidity_check: bool,
    ) -> Result<Address> {
        ZoneFeeManager::new().collect_fee_pre_tx(&self.provider, fee_payer, user_token, max_amount)
    }

    fn collect_fee_post_tx(
        &self,
        fee_payer: Address,
        actual_spending: U256,
        refund_amount: U256,
        fee_token: Address,
        beneficiary: Address,
    ) -> Result<U256> {
        ZoneFeeManager::new().collect_fee_post_tx(
            fee_payer,
            actual_spending,
            refund_amount,
            fee_token,
            beneficiary,
        )
    }
}
