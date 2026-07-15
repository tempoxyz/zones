//! Zone block executor.
//!
//! A simplified block executor for zone nodes that wraps [`EthBlockExecutor`] directly.
//! Unlike the Tempo L1 [`TempoBlockExecutor`], this executor does **not** enforce subblock
//! ordering, shared-gas accounting, or the end-of-block subblock metadata system transaction.

use alloy_consensus::transaction::TxHashRef;
use alloy_evm::{
    Database, Evm, RecoveredTx,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, GasOutput},
    eth::{EthBlockExecutor, EthTxResult},
};
use reth_evm::block::StateDB;
use reth_revm::Inspector;
use tempo_chainspec::TempoChainSpec;
use tempo_evm::{TempoBlockExecutionCtx, TempoReceiptBuilder};
use tempo_primitives::{TempoReceipt, TempoTxEnvelope, TempoTxType};
use tempo_revm::evm::TempoContext;

use crate::{ZoneEvm, tx_context};

/// Simplified block executor for zone nodes.
///
/// Wraps [`EthBlockExecutor`] without any subblock validation, gas-section tracking,
/// or end-of-block metadata system transaction requirements.
pub struct ZoneBlockExecutor<'a, DB: Database, I> {
    inner: EthBlockExecutor<'a, ZoneEvm<DB, I>, &'a TempoChainSpec, TempoReceiptBuilder>,
}

impl<'a, DB, I> ZoneBlockExecutor<'a, DB, I>
where
    DB: StateDB,
    I: Inspector<TempoContext<DB>>,
{
    pub(crate) fn new(
        evm: ZoneEvm<DB, I>,
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
}

impl<'a, DB, I> BlockExecutor for ZoneBlockExecutor<'a, DB, I>
where
    DB: StateDB,
    I: Inspector<TempoContext<DB>>,
{
    type Transaction = TempoTxEnvelope;
    type Receipt = TempoReceipt;
    type Evm = ZoneEvm<DB, I>;
    type Result = EthTxResult<<Self::Evm as Evm>::HaltReason, TempoTxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, recovered) = tx.into_parts();

        let _tx_hash_guard = tx_context::set_current_tx_hash(*recovered.tx().tx_hash());
        self.inner
            .execute_transaction_without_commit((tx_env, recovered))
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.inner.commit_transaction(output)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        self.inner.finish()
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
