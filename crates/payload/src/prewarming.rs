//! Builder-local `advanceTempo` cache prewarming.
//!
//! Each queued deposit is executed in an independent throwaway Zone builder. Execution output is
//! discarded; only exact-L1 reads populate the cache shared with canonical execution.
//!
//! A forced exit checks its global queue position before its L1 reads, so its worker starts from
//! a processed deposit number advanced by the entry's index in the queue.

use crate::builder::build_advance_tempo_tx_owned;
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{B256, U256};
use reth_errors::ProviderError;
use reth_evm::{ConfigureEvm, Database, execute::BlockBuilder};
use reth_primitives_traits::SealedHeader;
use reth_revm::{
    Database as RevmDatabase, DatabaseCommit, State,
    cancelled::ManualCancel,
    database::StateProviderDatabase,
    primitives::AddressMap,
    state::{Account, EvmStorageSlot},
};
use reth_storage_api::StateProviderFactory;
use reth_tasks::TaskExecutor;
use std::{error::Error, sync::Arc};
use tempo_evm::TempoNextBlockEnvAttributes;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::DepositType;
use zone_evm::ZoneEvmConfig;
use zone_l1::PreparedL1Block;
use zone_precompiles::inbox;
use zone_primitives::constants::ZONE_INBOX_ADDRESS;

/// Immutable canonical inputs used to construct isolated prewarming workers.
pub(crate) struct PrewarmingExecutionContext<Provider> {
    pub(crate) provider: Provider,
    pub(crate) evm_config: ZoneEvmConfig,
    pub(crate) parent_hash: B256,
    pub(crate) parent_header: SealedHeader<TempoHeader>,
    pub(crate) next_block_env_attributes: TempoNextBlockEnvAttributes,
    pub(crate) chain_id: u64,
}

impl<Provider> PrewarmingExecutionContext<Provider>
where
    Provider: StateProviderFactory + Clone + 'static,
{
    /// Enqueue one disposable simulation per deposit directly on the prewarming pool.
    pub(crate) fn start(
        self,
        task_executor: &TaskExecutor,
        prepared: &PreparedL1Block,
    ) -> AdvanceTempoPrewarming {
        let handle = AdvanceTempoPrewarming::default();
        if prepared.queued_deposits.is_empty() {
            return handle;
        }

        let context = Arc::new(self);
        let pool = task_executor.prewarming_pool();
        let mut decryptions = prepared.decryptions.iter();

        // The pool bounds concurrency; cancelled queued jobs skip EVM initialization.
        for (index, deposit) in prepared.queued_deposits.iter().enumerate() {
            // Both encrypted entry types consume a witness; bounce-backs do not.
            let decryptions = matches!(
                deposit.depositType,
                DepositType::Deposit | DepositType::ForcedExit
            )
            .then(|| decryptions.next().cloned())
            .flatten()
            .into_iter()
            .collect();
            // Canonical execution checks each forced exit against its real queue position.
            let queue_offset = match deposit.depositType {
                DepositType::ForcedExit => index as u64,
                _ => 0,
            };
            let partial = PreparedL1Block {
                header: prepared.header.clone(),
                enabled_tokens: prepared.enabled_tokens.clone(),
                queued_deposits: vec![deposit.clone()],
                decryptions,
                follows_checkpoint_blocks: false,
            };
            let context = context.clone();
            let cancel = handle.cancel.clone();
            pool.spawn(move || {
                if !cancel.is_cancelled() {
                    let _ = context.prewarm_deposit(partial, queue_offset);
                }
            });
        }

        handle
    }

    fn prewarm_deposit(
        &self,
        partial: PreparedL1Block,
        queue_offset: u64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let state =
            StateProviderDatabase::new(self.provider.state_by_block_hash(self.parent_hash)?);
        let mut db = State::builder()
            .with_database(Box::new(state) as Box<dyn Database<Error = ProviderError>>)
            .with_bundle_update()
            .build();
        if queue_offset > 0 {
            advance_processed_deposit_number(&mut db, queue_offset)?;
        }

        let mut worker = self.evm_config.builder_for_next_block(
            &mut db,
            &self.parent_header,
            self.next_block_env_attributes.clone(),
        )?;
        worker.apply_pre_execution_changes()?;

        // Queue-hash validation may fail, but preceding reads have already warmed the shared cache.
        _ = worker
            .executor_mut()
            .execute_transaction_without_commit(build_advance_tempo_tx_owned(
                partial,
                self.chain_id,
            ));
        Ok(())
    }
}

/// Moves the throwaway inbox cursor forward so the single-entry block sees its real position.
fn advance_processed_deposit_number<DB: RevmDatabase + DatabaseCommit>(
    db: &mut DB,
    offset: u64,
) -> Result<(), DB::Error> {
    let slot = inbox::slots::PROCESSED_DEPOSIT_NUMBER;
    let processed = db.storage(ZONE_INBOX_ADDRESS, slot)?;
    let mut account = Account::from(db.basic(ZONE_INBOX_ADDRESS)?.unwrap_or_default());
    account.mark_touch();
    account.storage.insert(
        slot,
        EvmStorageSlot::new_changed(
            processed,
            processed.saturating_add(U256::from(offset)),
            Default::default(),
        ),
    );
    db.commit(AddressMap::from_iter([(ZONE_INBOX_ADDRESS, account)]));
    Ok(())
}

/// Cancels queued work that has not started when dropped.
#[derive(Debug, Default)]
pub(crate) struct AdvanceTempoPrewarming {
    cancel: ManualCancel,
}

impl Drop for AdvanceTempoPrewarming {
    fn drop(&mut self) {
        self.cancel.clone().cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_revm::{
        db::{CacheDB, EmptyDB},
        state::AccountInfo,
    };

    #[test]
    fn advances_only_the_processed_deposit_number() {
        let hash_slot = inbox::slots::PROCESSED_DEPOSIT_QUEUE_HASH;
        let number_slot = inbox::slots::PROCESSED_DEPOSIT_NUMBER;
        let info = AccountInfo {
            nonce: 1,
            ..Default::default()
        };
        let mut cache = CacheDB::new(EmptyDB::default());
        cache.insert_account_info(ZONE_INBOX_ADDRESS, info);
        cache
            .insert_account_storage(ZONE_INBOX_ADDRESS, hash_slot, U256::from(0xabc))
            .unwrap();
        cache
            .insert_account_storage(ZONE_INBOX_ADDRESS, number_slot, U256::from(7))
            .unwrap();
        let mut db = State::builder()
            .with_database(cache)
            .with_bundle_update()
            .build();

        advance_processed_deposit_number(&mut db, 3).unwrap();

        assert_eq!(
            db.storage(ZONE_INBOX_ADDRESS, number_slot).unwrap(),
            U256::from(10)
        );
        assert_eq!(
            db.storage(ZONE_INBOX_ADDRESS, hash_slot).unwrap(),
            U256::from(0xabc)
        );
        assert_eq!(db.basic(ZONE_INBOX_ADDRESS).unwrap().unwrap().nonce, 1);
    }
}
