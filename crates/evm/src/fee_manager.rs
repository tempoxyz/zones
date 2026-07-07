//! Zone protocol fee-manager adapter.

use alloy_primitives::{Address, U256};
use core::fmt::Debug;
use revm::{Database, context::Journal};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{error::Result as TempoResult, storage::StorageActions};
use tempo_revm::{ProtocolFeeManager, TempoStateAccess, TempoTxEnv};
use zone_precompiles::{ZoneFeeManager, ZonePortalReader};

/// Protocol fee manager for zones.
///
/// It preserves Tempo transaction fee semantics but validates fee tokens
/// against the backing ZonePortal and settles fees directly in the user's token.
#[derive(Debug, Clone)]
pub(crate) struct ZoneProtocolFeeManager<P> {
    inner: ZoneFeeManager<P>,
}

impl<P: ZonePortalReader> ZoneProtocolFeeManager<P> {
    pub(crate) const fn new(provider: P) -> Self {
        Self {
            inner: ZoneFeeManager::new(provider),
        }
    }
}

impl<DB, P> ProtocolFeeManager<DB> for ZoneProtocolFeeManager<P>
where
    DB: Database,
    P: ZonePortalReader + Debug,
{
    fn get_fee_token(
        &self,
        journal: &mut Journal<DB>,
        tx: &TempoTxEnv,
        fee_payer: Address,
        spec: TempoHardfork,
        actions: StorageActions,
    ) -> TempoResult<Address> {
        let fee_token = <Journal<DB> as TempoStateAccess<((), ())>>::get_fee_token(
            journal,
            tx,
            fee_payer,
            spec,
            actions.clone(),
        )?;

        <Journal<DB> as TempoStateAccess<((), ())>>::with_read_only_storage_ctx(
            journal,
            spec,
            actions,
            || self.inner.ensure_token_enabled_current(fee_token),
        )?;

        Ok(fee_token)
    }

    fn collect_fee_pre_tx(
        &self,
        fee_payer: Address,
        user_token: Address,
        max_amount: U256,
        beneficiary: Address,
        skip_liquidity_check: bool,
    ) -> TempoResult<Address> {
        self.inner.collect_fee_pre_tx(
            fee_payer,
            user_token,
            max_amount,
            beneficiary,
            skip_liquidity_check,
        )
    }

    fn collect_fee_post_tx(
        &self,
        fee_payer: Address,
        actual_spending: U256,
        refund_amount: U256,
        fee_token: Address,
        beneficiary: Address,
    ) -> TempoResult<U256> {
        self.inner.collect_fee_post_tx(
            fee_payer,
            actual_spending,
            refund_amount,
            fee_token,
            beneficiary,
        )
    }
}
