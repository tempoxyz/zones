//! Zone block executor.
//!
//! A simplified block executor for zone nodes that wraps [`EthBlockExecutor`] directly.
//! Unlike the Tempo L1 [`TempoBlockExecutor`], this executor does **not** enforce subblock
//! ordering, shared-gas accounting, or the end-of-block subblock metadata system transaction.

use alloy_consensus::transaction::TxHashRef;
use alloy_evm::{
    Database, Evm, RecoveredTx,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
    eth::{EthBlockExecutor, EthTxResult},
};
use reth_evm::block::StateDB;
use reth_revm::Inspector;
use revm::context::{ContextTr, JournalTr, Transaction};
use tempo_chainspec::TempoChainSpec;
use tempo_evm::{TempoBlockExecutionCtx, TempoReceiptBuilder, evm::TempoEvm};
use tempo_precompiles::{TIP_FEE_MANAGER_ADDRESS, tip_fee_manager::TipFeeManager};
use tempo_primitives::{TempoReceipt, TempoTxEnvelope, TempoTxType};
use tempo_revm::{TempoStateAccess, evm::TempoContext};

use crate::tx_context;

/// Simplified block executor for zone nodes.
///
/// Wraps [`EthBlockExecutor`] without any subblock validation, gas-section tracking,
/// or end-of-block metadata system transaction requirements.
pub(crate) struct ZoneBlockExecutor<'a, DB: Database, I> {
    inner: EthBlockExecutor<'a, TempoEvm<DB, I>, &'a TempoChainSpec, TempoReceiptBuilder>,
}

impl<'a, DB, I> ZoneBlockExecutor<'a, DB, I>
where
    DB: StateDB,
    I: Inspector<TempoContext<DB>>,
{
    pub(crate) fn new(
        evm: TempoEvm<DB, I>,
        ctx: TempoBlockExecutionCtx<'a>,
        chain_spec: &'a TempoChainSpec,
    ) -> Self {
        Self {
            inner: EthBlockExecutor::new(
                evm,
                ctx.inner,
                chain_spec,
                TempoReceiptBuilder::default(),
            ),
        }
    }

    /// Overrides `validatorTokens[beneficiary]` to match the resolved fee token
    /// so the handler skips FeeAMM.
    fn override_validator_token(&mut self) {
        let ctx = self.inner.evm.ctx_mut();
        let fee_payer = ctx.tx.fee_payer().unwrap_or(ctx.tx.caller());
        let spec = ctx.cfg.spec;

        let fee_token = match ctx.journaled_state.get_fee_token(&ctx.tx, fee_payer, spec) {
            Ok(token) => token,
            Err(_) => return,
        };

        let beneficiary = ctx.block.beneficiary;
        let slot = TipFeeManager::new().validator_tokens[beneficiary].slot();

        let _ = ctx.journal_mut().load_account(TIP_FEE_MANAGER_ADDRESS);
        let _ = ctx.journal_mut().sstore(
            TIP_FEE_MANAGER_ADDRESS,
            slot,
            fee_token.into_word().into(),
        );
    }
}

impl<'a, DB, I> BlockExecutor for ZoneBlockExecutor<'a, DB, I>
where
    DB: StateDB,
    I: Inspector<TempoContext<DB>>,
{
    type Transaction = TempoTxEnvelope;
    type Receipt = TempoReceipt;
    type Evm = TempoEvm<DB, I>;
    type Result = EthTxResult<<Self::Evm as Evm>::HaltReason, TempoTxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, recovered) = tx.into_parts();

        // Override the validator's fee token preference to match this
        // transaction's resolved fee token, so the handler skips FeeAMM.
        self.override_validator_token();

        let _tx_hash_guard = tx_context::set_current_tx_hash(*recovered.tx().tx_hash());
        self.inner
            .execute_transaction_without_commit((tx_env, recovered))
    }

    fn commit_transaction(&mut self, output: Self::Result) -> Result<u64, BlockExecutionError> {
        let gas_used = self.inner.commit_transaction(output)?;

        // Collect revert logs (same as Tempo L1 executor).
        let logs = self.inner.evm.take_revert_logs();
        if !logs.is_empty() {
            self.inner
                .receipts
                .last_mut()
                .expect("receipt was just pushed")
                .logs
                .extend(logs);
        }

        Ok(gas_used)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        self.inner.finish()
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.inner.set_state_hook(hook)
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use tempo_precompiles::{
        DEFAULT_FEE_TOKEN, TIP_FEE_MANAGER_ADDRESS,
        storage::{ContractStorage, Handler, StorageCtx, hashmap::HashMapStorageProvider},
        test_util::TIP20Setup,
        tip_fee_manager::{TipFeeManager, amm::PoolKey},
    };

    /// Simulates the zone executor's per-tx validator token override and runs
    /// the full fee lifecycle across multiple TIP-20 tokens, verifying:
    ///
    /// 1. Default validator token is PATH_USD (no explicit preference set).
    /// 2. No FeeAMM liquidity exists for any token pair.
    /// 3. Paying fees in betaUSD, gammaUSD, and pathUSD all succeed when the
    ///    validator token is overridden per-tx.
    /// 4. Fees are credited in the user's token (no conversion).
    /// 5. FeeAMM pool reserves remain zero throughout.
    #[test]
    fn multi_token_fees_with_validator_override() -> eyre::Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        let admin = Address::random();
        let user = Address::random();
        let sequencer = Address::random();

        StorageCtx::enter(&mut storage, || {
            // Deploy three tokens.
            let path_usd = TIP20Setup::create("PathUSD", "pUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000_000u64))
                .with_approval(user, TIP_FEE_MANAGER_ADDRESS, U256::MAX)
                .apply()?;
            let beta_usd = TIP20Setup::create("BetaUSD", "bUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000_000u64))
                .with_approval(user, TIP_FEE_MANAGER_ADDRESS, U256::MAX)
                .apply()?;
            let gamma_usd = TIP20Setup::create("GammaUSD", "gUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000_000u64))
                .with_approval(user, TIP_FEE_MANAGER_ADDRESS, U256::MAX)
                .apply()?;

            let fee_manager = TipFeeManager::new();

            // 1. Validator token defaults to PATH_USD.
            assert_eq!(
                fee_manager.get_validator_token(sequencer)?,
                DEFAULT_FEE_TOKEN
            );

            // 2. No FeeAMM pools exist.
            for (a, b) in [
                (beta_usd.address(), DEFAULT_FEE_TOKEN),
                (gamma_usd.address(), DEFAULT_FEE_TOKEN),
                (beta_usd.address(), gamma_usd.address()),
            ] {
                let pool = fee_manager.pools[PoolKey::new(a, b).get_id()].read()?;
                assert_eq!(pool.reserve_user_token, 0);
                assert_eq!(pool.reserve_validator_token, 0);
            }

            // 3. Three transactions, each paying in a different token.
            let txs = [
                (
                    beta_usd.address(),
                    U256::from(5_000u64),
                    U256::from(3_000u64),
                ),
                (
                    gamma_usd.address(),
                    U256::from(8_000u64),
                    U256::from(7_000u64),
                ),
                (
                    path_usd.address(),
                    U256::from(4_000u64),
                    U256::from(2_000u64),
                ),
            ];

            let mut fee_manager = TipFeeManager::new();
            for (token, max, used) in &txs {
                // Zone executor override: validatorTokens[sequencer] = fee_token.
                fee_manager.validator_tokens[sequencer].write(*token)?;

                fee_manager.collect_fee_pre_tx(user, *token, *max, sequencer)?;
                fee_manager.collect_fee_post_tx(user, *used, *max - *used, *token, sequencer)?;
            }

            // 4. Fees credited per-token — no conversion happened.
            for (token, _, used) in &txs {
                let collected = fee_manager.collected_fees[sequencer][*token].read()?;
                assert_eq!(collected, *used, "fees should be credited in {token}");
            }

            // 5. FeeAMM pools still empty — never touched.
            for (a, b) in [
                (beta_usd.address(), DEFAULT_FEE_TOKEN),
                (gamma_usd.address(), DEFAULT_FEE_TOKEN),
                (beta_usd.address(), gamma_usd.address()),
            ] {
                let pool = fee_manager.pools[PoolKey::new(a, b).get_id()].read()?;
                assert_eq!(
                    pool.reserve_user_token, 0,
                    "pool {a}-{b} user reserve should be 0"
                );
                assert_eq!(
                    pool.reserve_validator_token, 0,
                    "pool {a}-{b} validator reserve should be 0"
                );
            }

            Ok(())
        })
    }

    /// Validator token slot computation is deterministic and the storage
    /// write produces the expected value when read back via TipFeeManager.
    #[test]
    fn validator_token_slot_roundtrip() -> eyre::Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        let sequencer = Address::random();
        let token = Address::random();

        StorageCtx::enter(&mut storage, || {
            let mut fee_manager = TipFeeManager::new();

            // Write via the Mapping handler (what the executor does via journal sstore).
            fee_manager.validator_tokens[sequencer].write(token)?;

            // Read back via TipFeeManager API.
            let read_back = fee_manager.get_validator_token(sequencer)?;
            assert_eq!(read_back, token);

            Ok(())
        })
    }
}
