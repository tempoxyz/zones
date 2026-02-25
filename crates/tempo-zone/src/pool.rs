//! Zone-level transaction pool and validation policy.
//!
//! [`ZoneTransactionPool`] wraps [`TempoTransactionPool`] and enforces a
//! [`ZoneTransactionPolicy`] on every transaction submission. Transactions that
//! violate the policy are rejected before they reach the inner pool.
//!
//! [`ZoneTransactionValidator`] is an alternative integration point that enforces
//! the same policy at the validator layer (requires `TempoTransactionPool` to be
//! made generic over the validator type).

use std::{any::Any, sync::Arc};

use alloy_consensus::Transaction;
use alloy_primitives::{Address, B256};
use reth_eth_wire_types::HandleMempoolData;
use reth_provider::ChangedAccount;
use reth_transaction_pool::{
    AddedTransactionOutcome, AllPoolTransactions, BestTransactions, BestTransactionsAttributes,
    BlockInfo, CanonicalStateUpdate, GetPooledTransactionLimit, NewBlobSidecar, PoolResult,
    PoolSize, PoolTransaction, PropagatedTransactions, TransactionEvents, TransactionOrigin,
    TransactionPool, TransactionPoolExt, TransactionValidationOutcome, TransactionValidator,
    ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError, PoolErrorKind, PoolTransactionError},
};
use tempo_transaction_pool::{TempoTransactionPool, validator::TempoTransactionValidator};

/// Policy flags that control which transaction types a zone accepts.
///
/// Applied by [`ZoneTransactionPool`] on every submission and by
/// [`ZoneTransactionValidator`] during validation.
#[derive(Debug, Clone)]
pub struct ZoneTransactionPolicy {
    /// When `true`, reject transactions whose [`TxKind`](alloy_primitives::TxKind) is
    /// [`Create`](alloy_primitives::TxKind::Create), preventing bare `CREATE` / `CREATE2`
    /// contract deployments via the transaction pool.
    pub deny_contract_creation: bool,
}

impl Default for ZoneTransactionPolicy {
    fn default() -> Self {
        Self {
            deny_contract_creation: true,
        }
    }
}

impl ZoneTransactionPolicy {
    /// Returns an error if `tx` violates this policy, `None` otherwise.
    fn check<T: Transaction + PoolTransaction>(&self, tx: &T) -> Option<PoolError> {
        if self.deny_contract_creation && tx.is_create() {
            return Some(PoolError::new(
                *tx.hash(),
                PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::other(
                    ContractCreationDenied,
                )),
            ));
        }
        None
    }
}

/// Transaction pool wrapper that enforces a [`ZoneTransactionPolicy`] on every
/// submission before delegating to the inner [`TempoTransactionPool`].
///
/// All query / listener / removal methods delegate directly to the inner pool.
pub struct ZoneTransactionPool<Client> {
    /// The wrapped Tempo transaction pool that handles AA routing and standard
    /// pool operations.
    inner: TempoTransactionPool<Client>,
    /// Policy applied to every incoming transaction.
    policy: ZoneTransactionPolicy,
}

impl<Client> ZoneTransactionPool<Client> {
    /// Create a new pool wrapping `inner` with the given `policy`.
    pub fn new(inner: TempoTransactionPool<Client>, policy: ZoneTransactionPolicy) -> Self {
        Self { inner, policy }
    }

    /// Returns a reference to the inner [`TempoTransactionPool`].
    pub fn inner(&self) -> &TempoTransactionPool<Client> {
        &self.inner
    }

    /// Returns a clone of the inner [`TempoTransactionPool`].
    ///
    /// Useful for passing to [`maintain_tempo_pool`](tempo_transaction_pool::maintain::maintain_tempo_pool)
    /// which requires `TempoTransactionPool<Client>` directly.
    pub fn tempo_pool(&self) -> TempoTransactionPool<Client>
    where
        Client: Clone,
    {
        self.inner.clone()
    }
}

impl<Client> Clone for ZoneTransactionPool<Client> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            policy: self.policy.clone(),
        }
    }
}

impl<Client> std::fmt::Debug for ZoneTransactionPool<Client> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZoneTransactionPool")
            .field("inner", &self.inner)
            .field("policy", &self.policy)
            .finish()
    }
}

impl<Client> TransactionPool for ZoneTransactionPool<Client>
where
    Client: reth_storage_api::StateProviderFactory
        + reth_chainspec::ChainSpecProvider<ChainSpec = tempo_chainspec::TempoChainSpec>
        + Send
        + Sync
        + 'static,
    tempo_transaction_pool::transaction::TempoPooledTransaction:
        reth_transaction_pool::EthPoolTransaction,
{
    type Transaction = <TempoTransactionPool<Client> as TransactionPool>::Transaction;

    fn pool_size(&self) -> PoolSize {
        self.inner.pool_size()
    }

    fn block_info(&self) -> BlockInfo {
        self.inner.block_info()
    }

    // -- Submission methods (policy-checked) ----------------------------------

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TransactionEvents> {
        if let Some(err) = self.policy.check(&transaction) {
            return Err(err);
        }
        self.inner
            .add_transaction_and_subscribe(origin, transaction)
            .await
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        if let Some(err) = self.policy.check(&transaction) {
            return Err(err);
        }
        self.inner.add_transaction(origin, transaction).await
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        if !self.policy.deny_contract_creation {
            return self.inner.add_transactions(origin, transactions).await;
        }

        let total = transactions.len();
        let mut output: Vec<Option<PoolResult<AddedTransactionOutcome>>> =
            (0..total).map(|_| None).collect();
        let mut allowed = Vec::with_capacity(total);
        let mut allowed_idx = Vec::with_capacity(total);

        for (i, tx) in transactions.into_iter().enumerate() {
            if let Some(err) = self.policy.check(&tx) {
                output[i] = Some(Err(err));
            } else {
                allowed_idx.push(i);
                allowed.push(tx);
            }
        }

        let inner_results = self.inner.add_transactions(origin, allowed).await;
        for (idx, result) in allowed_idx.into_iter().zip(inner_results) {
            output[idx] = Some(result);
        }

        output
            .into_iter()
            .map(|o| o.expect("all slots filled"))
            .collect()
    }

    // -- Listeners & events ---------------------------------------------------

    fn transaction_event_listener(&self, tx_hash: B256) -> Option<TransactionEvents> {
        self.inner.transaction_event_listener(tx_hash)
    }

    fn all_transactions_event_listener(
        &self,
    ) -> reth_transaction_pool::AllTransactionsEvents<Self::Transaction> {
        self.inner.all_transactions_event_listener()
    }

    fn pending_transactions_listener_for(
        &self,
        kind: reth_transaction_pool::TransactionListenerKind,
    ) -> tokio::sync::mpsc::Receiver<B256> {
        self.inner.pending_transactions_listener_for(kind)
    }

    fn blob_transaction_sidecars_listener(&self) -> tokio::sync::mpsc::Receiver<NewBlobSidecar> {
        self.inner.blob_transaction_sidecars_listener()
    }

    fn new_transactions_listener_for(
        &self,
        kind: reth_transaction_pool::TransactionListenerKind,
    ) -> tokio::sync::mpsc::Receiver<reth_transaction_pool::NewTransactionEvent<Self::Transaction>>
    {
        self.inner.new_transactions_listener_for(kind)
    }

    // -- Pooled transaction queries -------------------------------------------

    fn pooled_transaction_hashes(&self) -> Vec<B256> {
        self.inner.pooled_transaction_hashes()
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<B256> {
        self.inner.pooled_transaction_hashes_max(max)
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pooled_transactions()
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pooled_transactions_max(max)
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<B256>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled> {
        self.inner.get_pooled_transaction_elements(tx_hashes, limit)
    }

    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[B256],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<<Self::Transaction as PoolTransaction>::Pooled>,
    ) {
        self.inner
            .append_pooled_transaction_elements(tx_hashes, limit, out)
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: B256,
    ) -> Option<reth_primitives_traits::Recovered<<Self::Transaction as PoolTransaction>::Pooled>>
    {
        self.inner.get_pooled_transaction_element(tx_hash)
    }

    // -- Best / pending / queued ----------------------------------------------

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        self.inner.best_transactions()
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        self.inner
            .best_transactions_with_attributes(best_transactions_attributes)
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pending_transactions()
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pending_transactions_max(max)
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.queued_transactions()
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        self.inner.pending_and_queued_txn_count()
    }

    // -- All transactions -----------------------------------------------------

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        self.inner.all_transactions()
    }

    fn all_transaction_hashes(&self) -> Vec<B256> {
        self.inner.all_transaction_hashes()
    }

    // -- Removal --------------------------------------------------------------

    fn remove_transactions(
        &self,
        hashes: Vec<B256>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.remove_transactions(hashes)
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<B256>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.remove_transactions_and_descendants(hashes)
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.remove_transactions_by_sender(sender)
    }

    fn retain_unknown<A: HandleMempoolData>(&self, announcement: &mut A) {
        self.inner.retain_unknown(announcement)
    }

    fn contains(&self, tx_hash: &B256) -> bool {
        self.inner.contains(tx_hash)
    }

    // -- Getters --------------------------------------------------------------

    fn get(&self, tx_hash: &B256) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get(tx_hash)
    }

    fn get_all(&self, txs: Vec<B256>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_all(txs)
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        self.inner.on_propagated(txs)
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_transactions_by_sender(sender)
    }

    fn get_pending_transactions_with_predicate(
        &self,
        predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner
            .get_pending_transactions_with_predicate(predicate)
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_pending_transactions_by_sender(sender)
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_queued_transactions_by_sender(sender)
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_highest_transaction_by_sender(sender)
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner
            .get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner
            .get_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_transactions_by_origin(origin)
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_pending_transactions_by_origin(origin)
    }

    fn unique_senders(&self) -> std::collections::HashSet<Address> {
        self.inner.unique_senders()
    }

    // -- Blob store -----------------------------------------------------------

    fn get_blob(
        &self,
        tx_hash: B256,
    ) -> Result<
        Option<Arc<alloy_eips::eip7594::BlobTransactionSidecarVariant>>,
        reth_transaction_pool::blobstore::BlobStoreError,
    > {
        self.inner.get_blob(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<B256>,
    ) -> Result<
        Vec<(
            B256,
            Arc<alloy_eips::eip7594::BlobTransactionSidecarVariant>,
        )>,
        reth_transaction_pool::blobstore::BlobStoreError,
    > {
        self.inner.get_all_blobs(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<B256>,
    ) -> Result<
        Vec<Arc<alloy_eips::eip7594::BlobTransactionSidecarVariant>>,
        reth_transaction_pool::blobstore::BlobStoreError,
    > {
        self.inner.get_all_blobs_exact(tx_hashes)
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<
        Vec<Option<alloy_eips::eip4844::BlobAndProofV1>>,
        reth_transaction_pool::blobstore::BlobStoreError,
    > {
        self.inner
            .get_blobs_for_versioned_hashes_v1(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<
        Option<Vec<alloy_eips::eip4844::BlobAndProofV2>>,
        reth_transaction_pool::blobstore::BlobStoreError,
    > {
        self.inner
            .get_blobs_for_versioned_hashes_v2(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<
        Vec<Option<alloy_eips::eip4844::BlobAndProofV2>>,
        reth_transaction_pool::blobstore::BlobStoreError,
    > {
        self.inner
            .get_blobs_for_versioned_hashes_v3(versioned_hashes)
    }
}

impl<Client> TransactionPoolExt for ZoneTransactionPool<Client>
where
    Client: reth_storage_api::StateProviderFactory
        + reth_chainspec::ChainSpecProvider<ChainSpec = tempo_chainspec::TempoChainSpec>
        + Send
        + Sync
        + 'static,
    tempo_transaction_pool::transaction::TempoPooledTransaction:
        reth_transaction_pool::EthPoolTransaction,
{
    type Block = <TempoTransactionPool<Client> as TransactionPoolExt>::Block;

    fn set_block_info(&self, info: BlockInfo) {
        self.inner.set_block_info(info)
    }

    fn on_canonical_state_change(&self, update: CanonicalStateUpdate<'_, Self::Block>) {
        self.inner.on_canonical_state_change(update)
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        self.inner.update_accounts(accounts)
    }

    fn delete_blob(&self, tx: B256) {
        self.inner.delete_blob(tx)
    }

    fn delete_blobs(&self, txs: Vec<B256>) {
        self.inner.delete_blobs(txs)
    }

    fn cleanup_blobs(&self) {
        self.inner.cleanup_blobs()
    }
}

/// Transaction validator that enforces a [`ZoneTransactionPolicy`] on top of the standard
/// Tempo validation pipeline.
///
/// The default implementations of `validate_transactions` and
/// `validate_transactions_with_origin` delegate to `validate_transaction`, so the policy
/// check applies to batch submissions as well.
///
/// **Note:** This validator is not currently wired into the node because
/// [`TempoTransactionPool`] is hardcoded to [`TempoTransactionValidator`]. Use
/// [`ZoneTransactionPool`] for the pool-level integration instead.
#[derive(Debug)]
pub struct ZoneTransactionValidator<Client> {
    /// The inner Tempo validator that performs standard Ethereum + Tempo-specific
    /// validation (nonce, balance, gas, AA authorizations, AMM liquidity, etc.).
    inner: TempoTransactionValidator<Client>,
    /// Policy applied before the inner validator runs — a failing check short-circuits
    /// with [`TransactionValidationOutcome::Invalid`].
    policy: ZoneTransactionPolicy,
}

impl<Client> ZoneTransactionValidator<Client> {
    /// Create a new validator wrapping `inner` with the given `policy`.
    pub fn new(inner: TempoTransactionValidator<Client>, policy: ZoneTransactionPolicy) -> Self {
        Self { inner, policy }
    }

    /// Returns a reference to the inner [`TempoTransactionValidator`].
    pub fn inner(&self) -> &TempoTransactionValidator<Client> {
        &self.inner
    }
}

impl<Client> TransactionValidator for ZoneTransactionValidator<Client>
where
    Client: reth_provider::ChainSpecProvider<ChainSpec = tempo_chainspec::spec::TempoChainSpec>
        + reth_storage_api::StateProviderFactory,
{
    type Transaction = <TempoTransactionValidator<Client> as TransactionValidator>::Transaction;
    type Block = <TempoTransactionValidator<Client> as TransactionValidator>::Block;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        if self.policy.deny_contract_creation && transaction.is_create() {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::other(ContractCreationDenied),
            );
        }
        self.inner.validate_transaction(origin, transaction).await
    }

    fn on_new_head_block(&self, new_tip_block: &reth_primitives_traits::SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block)
    }
}

/// Error returned when a transaction attempts contract creation but the zone policy forbids it.
#[derive(Debug, thiserror::Error)]
#[error("contract creation transactions are not permitted by zone policy")]
pub struct ContractCreationDenied;

impl PoolTransactionError for ContractCreationDenied {
    fn is_bad_transaction(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
