//! Adapter between Tempo's protocol fee hooks and the zone fee-manager precompile.

use alloy_primitives::{Address, U256};
use revm::{Database, context::Journal};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{error::Result, storage::StorageActions};
use tempo_revm::{ProtocolFeeManager, TempoStateAccess, TempoTx, TempoTxEnv};
use zone_l1::state::PolicyProvider;
use zone_precompiles::{ZoneConfigReader, ZoneFeeManager, ZoneTip403ProxyRegistry};

/// Zone implementation of Tempo's internal protocol fee hooks.
#[derive(Clone)]
pub(crate) struct ZoneProtocolFeeManager<P> {
    provider: P,
    registry: Option<ZoneTip403ProxyRegistry<PolicyProvider>>,
}

impl<P> core::fmt::Debug for ZoneProtocolFeeManager<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ZoneProtocolFeeManager")
            .finish_non_exhaustive()
    }
}

impl<P> ZoneProtocolFeeManager<P> {
    pub(crate) const fn new(
        provider: P,
        registry: Option<ZoneTip403ProxyRegistry<PolicyProvider>>,
    ) -> Self {
        Self { provider, registry }
    }
}

impl<DB, P> ProtocolFeeManager<DB> for ZoneProtocolFeeManager<P>
where
    DB: Database,
    P: ZoneConfigReader,
{
    fn get_fee_token(
        &self,
        journal: &mut Journal<DB>,
        tx: &TempoTxEnv,
        _fee_payer: Address,
        spec: TempoHardfork,
        actions: StorageActions,
    ) -> Result<Address> {
        if let Some(token) = tx.fee_token() {
            return Ok(token);
        }

        journal.with_read_only_storage_ctx(spec, actions, || {
            ZoneFeeManager::new().default_fee_token(&self.provider)
        })
    }

    fn collect_fee_pre_tx(
        &self,
        fee_payer: Address,
        user_token: Address,
        max_amount: U256,
        _beneficiary: Address,
        _skip_liquidity_check: bool,
    ) -> Result<Address> {
        ZoneFeeManager::new().collect_fee_pre_tx(
            &self.provider,
            self.registry.as_ref(),
            fee_payer,
            user_token,
            max_amount,
        )
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
