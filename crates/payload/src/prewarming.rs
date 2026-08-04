//! Builder-local `advanceTempo` cache prewarming.
//!
//! Workers execute one-deposit `advanceTempo` calls against independent throwaway Zone builders.
//! Their state and execution output are discarded; the intended shared effect is warming exact-L1
//! cache entries through the normal Zone EVM path.

use crate::build_advance_tempo_tx;
use alloy_evm::{
    EvmFactory,
    block::{BlockExecutor, BlockExecutorFactory},
    revm::context_interface::block::Block as RevmBlock,
};
use alloy_primitives::B256;
use reth_errors::ProviderError;
use reth_evm::{BlockEnvFor, ConfigureEvm, Database, execute::BlockBuilder};
use reth_primitives_traits::SealedHeader;
use reth_revm::{State, cancelled::ManualCancel, database::StateProviderDatabase};
use reth_storage_api::StateProviderFactory;
use reth_tasks::TaskExecutor;
use std::{
    error::Error,
    sync::{Arc, mpsc},
};
use tempo_evm::TempoNextBlockEnvAttributes;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::DepositType;
use zone_l1::PreparedL1Block;

/// Immutable canonical inputs used to construct isolated prewarming workers.
pub(crate) struct PrewarmingExecutionContext<Provider, EvmConfig> {
    pub(crate) provider: Provider,
    pub(crate) evm_config: EvmConfig,
    pub(crate) task_executor: TaskExecutor,
    pub(crate) l1_fetch_concurrency: usize,
    pub(crate) parent_hash: B256,
    pub(crate) parent_header: SealedHeader<TempoHeader>,
    pub(crate) next_block_env_attributes: TempoNextBlockEnvAttributes,
    pub(crate) prepared: PreparedL1Block,
}

impl<Provider, EvmConfig> PrewarmingExecutionContext<Provider, EvmConfig>
where
    Provider: StateProviderFactory + Clone + 'static,
    EvmConfig: ConfigureEvm<
            Primitives = tempo_primitives::TempoPrimitives,
            NextBlockEnvCtx = TempoNextBlockEnvAttributes,
        > + 'static,
    <EvmConfig::BlockExecutorFactory as BlockExecutorFactory>::EvmFactory:
        EvmFactory<Tx = tempo_revm::TempoTxEnv>,
    BlockEnvFor<EvmConfig>: RevmBlock,
{
    /// Start a bounded coordinator that dispatches deposits in canonical queue order.
    ///
    /// Returns immediately; the handle stops further dispatch when dropped without waiting for
    /// already-running workers.
    pub(crate) fn start(self) -> AdvanceTempoPrewarming {
        let prewarming = AdvanceTempoPrewarming::default();
        let num_deposits = self.prepared.queued_deposits.len();
        let pool = self.task_executor.prewarming_pool();
        let limit = self
            .l1_fetch_concurrency
            .saturating_sub(1)
            .min(pool.current_num_threads())
            .min(num_deposits);
        if limit == 0 {
            return prewarming;
        }

        let context = Arc::new(self);
        let cancel = prewarming.cancel_worker();
        let task_executor = context.task_executor.clone();
        task_executor.spawn_blocking_named("zone-advance-tempo-prewarm", move || {
            Self::coordinate(context, limit, cancel);
        });
        prewarming
    }

    /// Dispatches deposits in queue order while keeping at most `limit` jobs active.
    fn coordinate(context: Arc<Self>, limit: usize, cancel: ManualCancel) {
        let pool = context.task_executor.prewarming_pool();
        let (tx, rx) = mpsc::channel();
        let mut decryptions = context.prepared.decryptions.iter();

        for (index, deposit) in context.prepared.queued_deposits.iter().enumerate() {
            // Wait only after filling the bounded window.
            if cancel.is_cancelled() || (index >= limit && rx.recv().is_err()) {
                break;
            }

            // FIFO dispatch lets the cursor get the entry without scanning the queued-deposit prefix.
            let decryptions = (deposit.depositType == DepositType::Encrypted)
                .then(|| decryptions.next().cloned())
                .flatten()
                .into_iter()
                .collect();
            let partial = PreparedL1Block {
                header: context.prepared.header.clone(),
                enabled_tokens: context.prepared.enabled_tokens.clone(),
                queued_deposits: vec![deposit.clone()],
                decryptions,
            };
            let (ctx, cancel, tx) = (context.clone(), cancel.clone(), tx.clone());

            pool.spawn(move || {
                // Do not initialize a worker after canonical `advanceTempo` has completed.
                if !cancel.is_cancelled() {
                    let _ = ctx.prewarm_deposit(&partial);
                }
                let _ = tx.send(());
            });
        }
    }

    fn prewarm_deposit(
        &self,
        partial: &PreparedL1Block,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let state =
            StateProviderDatabase::new(self.provider.state_by_block_hash(self.parent_hash)?);
        let mut db = State::builder()
            .with_database(Box::new(state) as Box<dyn Database<Error = ProviderError>>)
            .with_bundle_update()
            .build();

        let mut worker = self.evm_config.builder_for_next_block(
            &mut db,
            &self.parent_header,
            self.next_block_env_attributes.clone(),
        )?;
        worker.apply_pre_execution_changes()?;

        // Partial queues normally revert at final queue-hash validation. Reads performed before
        // that check have already warmed the shared cache, and the throwaway state is discarded.
        _ = worker
            .executor_mut()
            .execute_transaction_without_commit(build_advance_tempo_tx(partial));
        Ok(())
    }
}

/// Stops further dispatch when canonical `advanceTempo` completes.
///
/// Dropping this handle does not wait for already-running workers.
#[derive(Debug, Default)]
pub(crate) struct AdvanceTempoPrewarming {
    cancel: ManualCancel,
}

impl AdvanceTempoPrewarming {
    fn cancel_worker(&self) -> ManualCancel {
        self.cancel.clone()
    }
}

impl Drop for AdvanceTempoPrewarming {
    fn drop(&mut self) {
        self.cancel.clone().cancel();
    }
}
