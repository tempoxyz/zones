//! Zone block executor.
//!
//! A simplified block executor for zone nodes that wraps [`EthBlockExecutor`] directly.
//! Unlike the Tempo L1 [`TempoBlockExecutor`], this executor does **not** enforce subblock
//! ordering, shared-gas accounting, or the end-of-block subblock metadata system transaction.
//!
//! ## Multi-token fee support
//!
//! The Tempo EVM handler calls `TipFeeManager::new().collect_fee_pre_tx()`
//! directly in Rust. When the user's fee token differs from the sequencer's
//! `validatorToken`, the handler routes through `FeeAMM` — which requires
//! liquidity pools that don't exist on zones.
//!
//! To bypass this, the executor writes `validatorTokens[beneficiary]` to match
//! the transaction's fee token before each transaction. The handler then sees
//! `validator_token == user_token` and skips the AMM path entirely, crediting
//! fees directly in the user's chosen token.

use alloy_consensus::transaction::TxHashRef;
use alloy_evm::{
    Database, Evm, RecoveredTx,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
    eth::{EthBlockExecutor, EthTxResult},
};
use alloy_primitives::U256;
use reth_evm::block::StateDB;
use reth_revm::Inspector;
use revm::context::{ContextTr, JournalTr};
use tempo_chainspec::TempoChainSpec;
use tempo_evm::{TempoBlockExecutionCtx, TempoReceiptBuilder, evm::TempoEvm};
use tempo_precompiles::{TIP_FEE_MANAGER_ADDRESS, tip_fee_manager::TipFeeManager};
use tempo_primitives::{TempoReceipt, TempoTxEnvelope, TempoTxType};
use tempo_revm::evm::TempoContext;

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

    /// Writes `validatorTokens[beneficiary] = fee_token` to the FeeManager
    /// storage so that the handler's `TipFeeManager` sees matching tokens and
    /// skips the FeeAMM swap path.
    fn set_validator_token_for_tx(&mut self, tx: &TempoTxEnvelope) {
        let fee_token = match tx.fee_token() {
            Some(token) => token,
            // Non-AA txs without explicit fee token resolve to PATH_USD via
            // get_fee_token — the default validator token is also PATH_USD,
            // so no override needed.
            None => return,
        };

        let ctx = self.inner.evm.ctx_mut();
        let beneficiary = ctx.block.beneficiary;
        let slot = TipFeeManager::new().validator_tokens[beneficiary].slot();

        let _ = ctx.journal_mut().load_account(TIP_FEE_MANAGER_ADDRESS);
        let _ = ctx.journal_mut().sstore(
            TIP_FEE_MANAGER_ADDRESS,
            slot,
            U256::from_be_bytes(fee_token.into_array()),
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
        // transaction's fee token, so the handler skips FeeAMM.
        self.set_validator_token_for_tx(recovered.tx());

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
