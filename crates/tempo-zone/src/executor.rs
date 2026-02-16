//! Zone block executor.
//!
//! A simplified block executor for zone nodes that wraps [`EthBlockExecutor`] directly.
//! Unlike the Tempo L1 [`TempoBlockExecutor`], this executor does **not** enforce subblock
//! ordering, shared-gas accounting, or the end-of-block subblock metadata system transaction.

use alloy_consensus::Transaction;
use alloy_evm::{
    Database, Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
    eth::{EthBlockExecutor, receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx}},
};
use reth_revm::{Inspector, State, context::result::ResultAndState};
use tempo_chainspec::TempoChainSpec;
use tempo_evm::{TempoBlockExecutionCtx, TempoHaltReason, evm::TempoEvm};
use tempo_primitives::{TempoReceipt, TempoTxEnvelope};
use tempo_revm::evm::TempoContext;

/// Local receipt builder for zone execution, mirrors the upstream `TempoReceiptBuilder`.
#[derive(Debug, Clone, Copy, Default)]
struct ZoneReceiptBuilder;

impl ReceiptBuilder for ZoneReceiptBuilder {
    type Transaction = TempoTxEnvelope;
    type Receipt = TempoReceipt;

    fn build_receipt<E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'_, Self::Transaction, E>,
    ) -> Self::Receipt {
        let ReceiptBuilderCtx {
            tx,
            result,
            cumulative_gas_used,
            ..
        } = ctx;
        TempoReceipt {
            tx_type: tx.tx_type(),
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.into_logs(),
        }
    }
}

/// Simplified block executor for zone nodes.
///
/// Wraps [`EthBlockExecutor`] without any subblock validation, gas-section tracking,
/// or end-of-block metadata system transaction requirements.
pub(crate) struct ZoneBlockExecutor<'a, DB: Database, I> {
    inner: EthBlockExecutor<
        'a,
        TempoEvm<&'a mut State<DB>, I>,
        &'a TempoChainSpec,
        ZoneReceiptBuilder,
    >,
}

impl<'a, DB, I> ZoneBlockExecutor<'a, DB, I>
where
    DB: Database,
    I: Inspector<TempoContext<&'a mut State<DB>>>,
{
    pub(crate) fn new(
        evm: TempoEvm<&'a mut State<DB>, I>,
        ctx: TempoBlockExecutionCtx<'a>,
        chain_spec: &'a TempoChainSpec,
    ) -> Self {
        Self {
            inner: EthBlockExecutor::new(
                evm,
                ctx.inner,
                chain_spec,
                ZoneReceiptBuilder,
            ),
        }
    }
}

impl<'a, DB, I> BlockExecutor for ZoneBlockExecutor<'a, DB, I>
where
    DB: Database,
    I: Inspector<TempoContext<&'a mut State<DB>>>,
{
    type Transaction = TempoTxEnvelope;
    type Receipt = TempoReceipt;
    type Evm = TempoEvm<&'a mut State<DB>, I>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<ResultAndState<TempoHaltReason>, BlockExecutionError> {
        self.inner.execute_transaction_without_commit(tx)
    }

    fn commit_transaction(
        &mut self,
        output: ResultAndState<TempoHaltReason>,
        tx: impl ExecutableTx<Self>,
    ) -> Result<u64, BlockExecutionError> {
        let gas_used = self.inner.commit_transaction(output, tx)?;

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
