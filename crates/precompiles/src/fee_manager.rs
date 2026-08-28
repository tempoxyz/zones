//! Zone fee manager.
//!
//! Zones keep the public Tempo FeeManager address and storage layout, but their
//! protocol fee path differs from Tempo L1: the sequencer accepts the user's fee
//! token directly, so fees never route through the Fee AMM or validator token
//! preference.

use alloy_primitives::{Address, U256};
use tempo_precompiles::{
    PrecompileEnv, TIP_FEE_MANAGER_ADDRESS,
    error::{Result, TempoPrecompileError},
    storage::Handler,
    tip20::TIP20Token,
};

/// Zone protocol fee manager.
///
/// This intentionally shares the canonical FeeManager precompile address and
/// storage with Tempo's [`TipFeeManager`](tempo_precompiles::tip_fee_manager::TipFeeManager),
/// but its protocol hooks credit the sequencer in the transaction fee token.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZoneFeeManager;

impl ZoneFeeManager {
    /// Creates a new zone fee manager handle.
    pub const fn new() -> Self {
        Self
    }

    /// Wraps the public FeeManager-compatible precompile for zone EVM registration.
    ///
    /// Contract-facing calls keep Tempo's FeeManager ABI and storage layout. The
    /// protocol fee lifecycle uses [`Self::collect_fee_pre_tx`] and
    /// [`Self::collect_fee_post_tx`] via the EVM fee-manager hook.
    pub fn create_precompile(env: &PrecompileEnv) -> alloy_evm::precompiles::DynPrecompile {
        tempo_precompiles::tip_fee_manager::TipFeeManager::create_precompile(env)
    }

    /// Collects the maximum possible fee before transaction execution.
    ///
    /// Unlike Tempo L1, zones do not inspect `validatorTokens` and do not reserve
    /// AMM liquidity. The user's fee token is escrowed directly in the FeeManager
    /// account and returned as the fee token for post-transaction settlement.
    pub fn collect_fee_pre_tx(
        &self,
        fee_payer: Address,
        fee_token: Address,
        max_amount: U256,
        _beneficiary: Address,
        _skip_liquidity_check: bool,
    ) -> Result<Address> {
        let mut token = TIP20Token::from_address(fee_token)?;
        token.ensure_transfer_authorized(fee_payer, TIP_FEE_MANAGER_ADDRESS)?;
        token.transfer_fee_pre_tx(fee_payer, max_amount)?;

        Ok(fee_token)
    }

    /// Settles the final fee after transaction execution.
    ///
    /// Refunds unused gas in the same token and credits the sequencer in that
    /// token. No Fee AMM swap is attempted.
    pub fn collect_fee_post_tx(
        &self,
        fee_payer: Address,
        actual_spending: U256,
        refund_amount: U256,
        fee_token: Address,
        beneficiary: Address,
    ) -> Result<U256> {
        let mut token = TIP20Token::from_address(fee_token)?;
        token.transfer_fee_post_tx(fee_payer, refund_amount, actual_spending)?;

        if !actual_spending.is_zero() {
            let mut fee_manager = tempo_precompiles::tip_fee_manager::TipFeeManager::new();
            let collected = fee_manager.collected_fees[beneficiary][fee_token].read()?;
            let collected = collected
                .checked_add(actual_spending)
                .ok_or_else(TempoPrecompileError::under_overflow)?;
            fee_manager.collected_fees[beneficiary][fee_token].write(collected)?;
        }

        Ok(actual_spending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_precompiles::{
        TIP_FEE_MANAGER_ADDRESS,
        storage::{ContractStorage, StorageCtx, hashmap::HashMapStorageProvider},
        test_util::TIP20Setup,
        tip_fee_manager::{TipFeeManager, amm::PoolKey},
    };

    #[test]
    fn zone_fees_credit_user_token_without_amm_or_validator_override() -> Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        let admin = Address::random();
        let user = Address::random();
        let sequencer = Address::random();

        StorageCtx::enter(&mut storage, || {
            let user_token = TIP20Setup::create("User USD", "uUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000u64))
                .with_approval(user, TIP_FEE_MANAGER_ADDRESS, U256::MAX)
                .apply()?;
            let validator_token = TIP20Setup::create("Validator USD", "vUSD", admin)
                .with_issuer(admin)
                .apply()?;

            let mut tempo_fee_manager = TipFeeManager::new();
            tempo_fee_manager.validator_tokens[sequencer].write(validator_token.address())?;

            let max_fee = U256::from(5_000u64);
            let actual_fee = U256::from(3_000u64);
            ZoneFeeManager::new().collect_fee_pre_tx(
                user,
                user_token.address(),
                max_fee,
                sequencer,
                false,
            )?;
            ZoneFeeManager::new().collect_fee_post_tx(
                user,
                actual_fee,
                max_fee - actual_fee,
                user_token.address(),
                sequencer,
            )?;

            let fee_manager = TipFeeManager::new();
            assert_eq!(
                fee_manager.collected_fees[sequencer][user_token.address()].read()?,
                actual_fee
            );
            assert_eq!(
                fee_manager.collected_fees[sequencer][validator_token.address()].read()?,
                U256::ZERO
            );
            assert_eq!(
                fee_manager.get_validator_token(sequencer)?,
                validator_token.address()
            );

            let pool = fee_manager.pools
                [PoolKey::new(user_token.address(), validator_token.address()).get_id()]
            .read()?;
            assert_eq!(pool.reserve_user_token, 0);
            assert_eq!(pool.reserve_validator_token, 0);

            Ok(())
        })
    }
}
