//! Zone payload builder.
//!
//! Builds zone blocks by executing `advanceTempo` system transactions (one per L1 block)
//! followed by pool transactions and a withdrawal batch finalization.

use crate::abi::{self, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};
use alloy_consensus::{Signed, Transaction, TxLegacy, TxReceipt, transaction::TxHashRef};
use alloy_eips::{BlockHashOrNumber, eip4895::Withdrawals};
use alloy_evm::{
    EvmFactory,
    block::{BlockExecutor, BlockExecutorFactory, TxResult},
    revm::context_interface::block::Block as RevmBlock,
};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rlp::Encodable;
use alloy_sol_types::{SolCall, SolEvent};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_errors::ProviderError;
use reth_evm::{
    BlockEnvFor, ConfigureEvm, Database, NextBlockEnvAttributes, TxEnvFor,
    execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionOutput},
};
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{BuilderContext, components::PayloadBuilderBuilder};
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderError};
use reth_payload_primitives::{BuiltPayloadExecutedBlock, PayloadAttributes};
use reth_primitives_traits::{AlloyBlockHeader as _, Recovered};
use reth_revm::{State, database::StateProviderDatabase};
use reth_storage_api::{
    BlockReader, HeaderProvider, ReceiptProvider, StateProvider, StateProviderFactory,
    TransactionsProvider,
};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, TransactionPool,
    error::InvalidPoolTransactionError,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tempo_chainspec::spec::TempoChainSpec;
use tempo_evm::TempoNextBlockEnvAttributes;
use tempo_payload_types::{EncodedBlock, TempoBuiltPayload};
use tempo_primitives::{
    TempoHeader, TempoReceipt, TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};
use tempo_transaction_pool::TempoTransactionPool;
use tracing::{error, info, warn};
use zone_l1::{PreparedL1Block, TempoStateExt};

use crate::{ZonePayloadAttributes, ZonePayloadTypes};

#[derive(Clone)]
struct RequestedWithdrawalContext {
    event: abi::ZoneOutbox::WithdrawalRequested,
    tx_hash: B256,
}

struct OpenBatchRequestedWithdrawals {
    previous_boundary_block: u64,
    requested_withdrawals: Vec<RequestedWithdrawalContext>,
}

pub const DEFAULT_WITHDRAWAL_BATCH_INTERVAL: Duration = Duration::from_secs(60);

/// Factory for constructing the zone payload builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZonePayloadFactory {
    withdrawal_batch_interval: Duration,
}

impl ZonePayloadFactory {
    pub fn new(withdrawal_batch_interval: Duration) -> Self {
        Self {
            withdrawal_batch_interval,
        }
    }
}

impl Default for ZonePayloadFactory {
    fn default() -> Self {
        Self::new(DEFAULT_WITHDRAWAL_BATCH_INTERVAL)
    }
}

impl<Node, EvmConfig> PayloadBuilderBuilder<Node, TempoTransactionPool<Node::Provider>, EvmConfig>
    for ZonePayloadFactory
where
    Node: FullNodeTypes,
    Node::Types: NodeTypes<
            Primitives = tempo_primitives::TempoPrimitives,
            ChainSpec = TempoChainSpec,
            Payload = ZonePayloadTypes,
        >,
    EvmConfig: ConfigureEvm<
            Primitives = tempo_primitives::TempoPrimitives,
            NextBlockEnvCtx = TempoNextBlockEnvAttributes,
        > + 'static,
    <EvmConfig::BlockExecutorFactory as BlockExecutorFactory>::EvmFactory:
        EvmFactory<Tx = tempo_revm::TempoTxEnv>,
    BlockEnvFor<EvmConfig>: RevmBlock,
{
    type PayloadBuilder = ZonePayloadBuilder<Node::Provider, EvmConfig>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: TempoTransactionPool<Node::Provider>,
        evm_config: EvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(ZonePayloadBuilder {
            pool,
            provider: ctx.provider().clone(),
            evm_config,
            withdrawal_batch_interval: self.withdrawal_batch_interval,
        })
    }
}

/// Zone payload builder that executes `advanceTempo` system txs + pool txs.
#[derive(Debug, Clone)]
pub struct ZonePayloadBuilder<Provider, EvmConfig> {
    /// Transaction pool for selecting pool txs to include in the block.
    pool: TempoTransactionPool<Provider>,
    /// State provider for reading chain state during block building.
    provider: Provider,
    /// Zone-specific EVM configuration (precompiles, hardfork spec, gas params).
    evm_config: EvmConfig,
    /// Maximum chain-time duration between withdrawal batch finalizations.
    withdrawal_batch_interval: Duration,
}

impl<Provider, EvmConfig> PayloadBuilder for ZonePayloadBuilder<Provider, EvmConfig>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = TempoChainSpec>
        + HeaderProvider<Header = TempoHeader>
        + BlockReader<
            Block = tempo_primitives::Block,
            Transaction = TempoTxEnvelope,
            Receipt = TempoReceipt,
        > + Clone
        + 'static,
    EvmConfig: ConfigureEvm<
            Primitives = tempo_primitives::TempoPrimitives,
            NextBlockEnvCtx = TempoNextBlockEnvAttributes,
        > + 'static,
    TxEnvFor<EvmConfig>: From<tempo_revm::TempoTxEnv>,
    <EvmConfig::BlockExecutorFactory as BlockExecutorFactory>::EvmFactory:
        EvmFactory<Tx = tempo_revm::TempoTxEnv>,
    BlockEnvFor<EvmConfig>: RevmBlock,
{
    type Attributes = ZonePayloadAttributes;
    type BuiltPayload = TempoBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        let BuildArguments {
            mut cached_reads,
            config,
            cancel,
            ..
        } = args;
        let PayloadConfig {
            parent_header,
            attributes,
            payload_id: _,
            parent_block_info: _,
        } = config;

        let start = Instant::now();

        // Read the current tempoBlockHash and tempoBlockNumber from TempoState storage
        // to validate the next L1 block we process is the expected successor.
        let sp = self.provider.state_by_block_hash(parent_header.hash())?;
        let stored_l1 = sp
            .tempo_num_hash()
            .map_err(|e| PayloadBuilderError::Internal(e.into()))?;
        let stored_l1_block_hash = stored_l1.hash;
        let expected_tempo_block_number = stored_l1.number + 1;

        info!(
            target: "zone::payload",
            %stored_l1_block_hash,
            expected_tempo_block_number,
            "TempoState current state"
        );

        let prepared = attributes.l1_block();

        // Validate chain continuity: the L1 block must be exactly tempoBlockNumber + 1
        // and its parent hash must match the stored tempoBlockHash.
        if prepared.header.inner.number != expected_tempo_block_number {
            error!(
                target: "zone::payload",
                got = prepared.header.inner.number,
                expected = expected_tempo_block_number,
                "L1 block number mismatch — chain continuity broken"
            );
            return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
                format!(
                    "L1 block number mismatch: got {} expected {}",
                    prepared.header.inner.number, expected_tempo_block_number
                ),
            )));
        }
        if prepared.header.inner.parent_hash != stored_l1_block_hash {
            error!(
                target: "zone::payload",
                got = %prepared.header.inner.parent_hash,
                expected = %stored_l1_block_hash,
                l1_block = prepared.header.inner.number,
                "L1 parent hash mismatch — chain continuity broken"
            );
            return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
                format!(
                    "L1 parent hash mismatch at block {}: got {} expected {}",
                    prepared.header.inner.number,
                    prepared.header.inner.parent_hash,
                    stored_l1_block_hash
                ),
            )));
        }

        let total_deposits = prepared.queued_deposits.len();

        info!(
            target: "zone::payload",
            zone_block = parent_header.number() + 1,
            l1_block = prepared.header.inner.number,
            deposits = total_deposits,
            enabled_tokens = prepared.enabled_tokens.len(),
            "Including advanceTempo system tx (chain continuity OK)"
        );

        let state_provider = self.provider.state_by_block_hash(parent_header.hash())?;
        let state_provider: Box<dyn StateProvider> = state_provider;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(
                Box::new(cached_reads.as_db_mut(state)) as Box<dyn Database<Error = ProviderError>>
            )
            .with_bundle_update()
            .build();

        let chain_spec = self.provider.chain_spec();

        let block_gas_limit = parent_header.gas_limit();

        let mut cumulative_gas_used = 0u64;
        let total_fees = U256::ZERO;
        let mut requested_withdrawals = Vec::new();

        let next_block_env_attributes = TempoNextBlockEnvAttributes {
            inner: NextBlockEnvAttributes {
                timestamp: attributes.timestamp(),
                suggested_fee_recipient: attributes.suggested_fee_recipient(),
                prev_randao: attributes.prev_randao(),
                gas_limit: block_gas_limit,
                parent_beacon_block_root: attributes.parent_beacon_block_root(),
                withdrawals: attributes.withdrawals().cloned().map(Withdrawals::new),
                extra_data: attributes.extra_data(),
                slot_number: attributes.slot_number(),
            },
            // Zones don't use L1 gas sections. These fields are required
            // by TempoNextBlockEnvAttributes but ignored by the zone executor.
            general_gas_limit: 0,
            shared_gas_limit: block_gas_limit,
            timestamp_millis_part: attributes.timestamp_millis_part(),
            consensus_context: None,
            subblock_fee_recipients: Default::default(),
        };
        let next_env = self
            .evm_config
            .next_evm_env(parent_header.header(), &next_block_env_attributes)
            .map_err(PayloadBuilderError::other)?;
        let base_fee = next_env.block_env.basefee();
        let block_number: u64 = next_env
            .block_env
            .number()
            .try_into()
            .expect("block number fits u64");

        let mut builder = self
            .evm_config
            .builder_for_next_block(&mut db, &parent_header, next_block_env_attributes)
            .map_err(PayloadBuilderError::other)?;

        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(%err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;

        // Execute advanceTempo system transaction — exactly one per zone block.
        {
            let advance_tx = build_advance_tempo_tx(prepared);
            let advance_tx_hash = *advance_tx.tx_hash();
            let mut reverted = false;
            match builder.execute_transaction_with_result_closure(advance_tx, |result| {
                let evm_result = result.result();
                if !evm_result.result.is_success() {
                    let revert_data = evm_result.result.output().cloned().unwrap_or_default();
                    error!(
                        target: "zone::payload",
                        l1_block = prepared.header.inner.number,
                        deposits = total_deposits,
                        is_halt = evm_result.result.is_halt(),
                        revert_data = %revert_data,
                        "advanceTempo system tx reverted on-chain"
                    );
                    reverted = true;
                }
            }) {
                Ok(_) if reverted => {
                    return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
                        format!(
                            "advanceTempo reverted at L1 block {}",
                            prepared.header.inner.number
                        ),
                    )));
                }
                Ok(_) => {}
                Err(err) => {
                    error!(
                        ?err,
                        l1_block = prepared.header.inner.number,
                        deposits = total_deposits,
                        "advanceTempo system tx failed"
                    );
                    return Err(PayloadBuilderError::evm(err));
                }
            }
            if let Some(receipt) = builder.executor().receipts().last() {
                collect_requested_withdrawals(
                    receipt,
                    advance_tx_hash,
                    &mut requested_withdrawals,
                )?;
            }
        }

        // Execute pool transactions
        // TODO: Use gas accounting from TempoPayloadBuilder (payment vs non-payment limits, etc.)
        let mut best_txs = self
            .pool
            .best_transactions_with_attributes(BestTransactionsAttributes::new(base_fee, None));

        while let Some(pool_tx) = best_txs.next() {
            // Contract creation (CREATE) transactions are not allowed on zones
            if pool_tx.transaction.is_create() {
                best_txs.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::Consensus(
                        reth_primitives_traits::transaction::error::InvalidTransactionError::TxTypeNotSupported,
                    ),
                );
                continue;
            }
            let gas_limit_left = block_gas_limit;
            if cumulative_gas_used + pool_tx.gas_limit() > gas_limit_left {
                best_txs.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::ExceedsGasLimit(
                        pool_tx.gas_limit(),
                        gas_limit_left.saturating_sub(cumulative_gas_used),
                    ),
                );
                continue;
            }

            if cancel.is_cancelled() {
                return Ok(BuildOutcome::Cancelled);
            }

            let tx_with_env = pool_tx.transaction.clone().into_with_tx_env();
            let tx_hash = *pool_tx.hash();
            match builder.execute_transaction(tx_with_env) {
                Ok(gas_used) => {
                    cumulative_gas_used += gas_used.tx_gas_used();
                    if let Some(receipt) = builder.executor().receipts().last() {
                        collect_requested_withdrawals(
                            receipt,
                            tx_hash,
                            &mut requested_withdrawals,
                        )?;
                    }
                }
                Err(reth_evm::block::BlockExecutionError::Validation(
                    reth_evm::block::BlockValidationError::InvalidTx { error, .. },
                )) => {
                    if !error.is_nonce_too_low() {
                        best_txs.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Consensus(
                                reth_primitives_traits::transaction::error::InvalidTransactionError::TxTypeNotSupported,
                            ),
                        );
                    }
                    continue;
                }
                Err(reth_evm::block::BlockExecutionError::Internal(
                    reth_evm::block::InternalBlockExecutionError::EVM { ref error, .. },
                )) if zone_precompiles::is_zone_rpc_error(&error.to_string()) => {
                    warn!(target: "zone::payload", %error, ?pool_tx, "skipping pool tx due to transient RPC error");
                    continue;
                }
                Err(err) => return Err(PayloadBuilderError::evm(err)),
            }
        }

        let open_batch =
            load_open_batch_requested_withdrawals(&self.provider, parent_header.number())?;
        let has_prior_withdrawals = !open_batch.requested_withdrawals.is_empty();
        let batch_interval_elapsed = withdrawal_batch_interval_elapsed(
            &self.provider,
            open_batch.previous_boundary_block,
            parent_header.header(),
            attributes.timestamp(),
            self.withdrawal_batch_interval,
        )?;

        // Finalize when there are prior (un-finalized) withdrawals to make visible on
        // L1 — folding in this block's withdrawals when present — or when the empty-batch
        // interval elapses so the L2 and L1 batch indexes stay in lockstep. A block that
        // has only current-block withdrawals and no prior defers them to the next block,
        // which joins consecutive withdrawal blocks into one batch (max +1 block latency).
        if has_prior_withdrawals || batch_interval_elapsed {
            let mut batch_withdrawals = open_batch.requested_withdrawals;
            batch_withdrawals.extend(requested_withdrawals);

            let encrypted_senders = batch_withdrawals
                .iter()
                .map(|request| {
                    if request.event.revealTo.is_empty() {
                        Ok(Bytes::new())
                    } else {
                        zone_precompiles::ecies::encrypt_authenticated_withdrawal(
                            request.event.revealTo.as_ref(),
                            request.event.sender,
                            request.tx_hash,
                        )
                        .map(Bytes::from)
                        .ok_or_else(|| {
                            PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
                                "failed to encrypt authenticated sender reveal for tx {}",
                                request.tx_hash
                            )))
                        })
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Finalize exactly the withdrawals we carry encrypted senders for. The contract
            // processes the oldest `count` pending FIFO; `batch_withdrawals` (prior ++ current)
            // equals all pending at this point, so this folds current in and leaves nothing
            // stranded.
            let count = U256::from(batch_withdrawals.len());
            let finalize_tx =
                build_finalize_withdrawal_batch_tx(count, block_number, encrypted_senders);
            let mut finalize_reverted = false;
            match builder.execute_transaction_with_result_closure(finalize_tx, |result| {
                let evm_result = result.result();
                if !evm_result.result.is_success() {
                    let revert_data = evm_result.result.output().cloned().unwrap_or_default();
                    error!(
                        target: "zone::payload",
                        block_number,
                        is_halt = evm_result.result.is_halt(),
                        revert_data = %revert_data,
                        "finalizeWithdrawalBatch system tx reverted on-chain"
                    );
                    finalize_reverted = true;
                }
            }) {
                Ok(_) if finalize_reverted => {
                    return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
                        format!("finalizeWithdrawalBatch reverted at zone block {block_number}"),
                    )));
                }
                Ok(_) => {}
                Err(err) => {
                    error!(?err, "finalizeWithdrawalBatch system tx failed");
                    return Err(PayloadBuilderError::evm(err));
                }
            }
        }

        let BlockBuilderOutcome {
            execution_result,
            hashed_state,
            trie_updates,
            block,
            block_access_list: _,
        } = builder.finish(&*state_provider, None)?;

        let requests = chain_spec
            .is_prague_active_at_timestamp(attributes.timestamp())
            .then_some(execution_result.requests.clone());

        let sealed_block = Arc::new(block.sealed_block().clone());
        let elapsed = start.elapsed();

        info!(
            number = sealed_block.number(),
            l1_block = prepared.header.number(),
            l1_hash = ?prepared.header.hash(),
            hash = ?sealed_block.hash(),
            gas_used = sealed_block.gas_used(),
            deposits = total_deposits,
            tx_count = sealed_block.body().transactions.len(),
            ?elapsed,
            "Built zone payload"
        );

        let recovered_block = Arc::new(block);
        let execution_block_encoded = EncodedBlock::default();
        let execution_block_size_estimate = execution_block_encoded
            .get_or_encode(sealed_block.as_ref())
            .len();
        let eth_payload = EthBuiltPayload::new(recovered_block.clone(), total_fees, requests, None);

        let execution_output = BlockExecutionOutput {
            result: execution_result,
            state: db.take_bundle(),
        };

        let executed_block = BuiltPayloadExecutedBlock {
            recovered_block,
            execution_output: Arc::new(execution_output),
            hashed_state: Arc::new(hashed_state),
            trie_updates: Arc::new(trie_updates),
        };

        let payload = TempoBuiltPayload::new(
            eth_payload,
            None,
            Some(executed_block),
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            execution_block_size_estimate,
            execution_block_encoded,
        );

        drop(db);
        // Zone payloads are deterministic (one L1 block = one zone block), so freeze
        // the payload to prevent reth from re-triggering try_build on the rebuild interval.
        // Without this, the next rebuild attempt would find the deposit queue empty.
        Ok(BuildOutcome::Freeze(payload))
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        MissingPayloadBehaviour::AwaitInProgress
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes, TempoHeader>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        self.try_build(BuildArguments::new(
            Default::default(),
            None,
            None,
            config,
            Default::default(),
            Default::default(),
        ))?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

/// Build the `finalizeWithdrawalBatch(count)` system transaction.
///
/// This must be the **last** transaction in each finalizing zone block. It calls
/// [`ZoneOutbox.finalizeWithdrawalBatch`](crate::abi::ZoneOutbox) which:
/// - Collects up to `count` pending withdrawals
/// - Builds the withdrawal hash chain (oldest outermost)
/// - Increments `withdrawalBatchIndex`
/// - Writes `_lastBatch` to state for proof access
/// - Emits `BatchFinalized`
///
/// `count` should match the number of withdrawals represented by `encrypted_senders`.
/// `block_number` must match the current zone block number.
pub(crate) fn build_finalize_withdrawal_batch_tx(
    count: U256,
    block_number: u64,
    encrypted_senders: Vec<Bytes>,
) -> Recovered<TempoTxEnvelope> {
    let calldata = abi::ZoneOutbox::finalizeWithdrawalBatchCall {
        count,
        blockNumber: block_number,
        encryptedSenders: encrypted_senders,
    }
    .abi_encode();

    let tx = TxLegacy {
        chain_id: None,
        nonce: 0,
        gas_price: 0,
        gas_limit: 0,
        to: ZONE_OUTBOX_ADDRESS.into(),
        value: U256::ZERO,
        input: calldata.into(),
    };

    Recovered::new_unchecked(
        TempoTxEnvelope::Legacy(Signed::new_unhashed(tx, TEMPO_SYSTEM_TX_SIGNATURE)),
        TEMPO_SYSTEM_TX_SENDER,
    )
}

/// Reconstruct withdrawal requests and boundary metadata for the currently open
/// batch from canonical history.
///
/// Scans committed blocks after the most recent `BatchFinalized` boundary up to
/// `parent_block_number`, collecting successful `WithdrawalRequested` logs in
/// block/transaction/log order. The block currently being built is not included;
/// callers append its newly executed requests before finalizing.
fn load_open_batch_requested_withdrawals<Provider>(
    provider: &Provider,
    parent_block_number: u64,
) -> Result<OpenBatchRequestedWithdrawals, PayloadBuilderError>
where
    Provider: ReceiptProvider<Receipt = TempoReceipt>
        + TransactionsProvider<Transaction = TempoTxEnvelope>,
{
    let empty_batch = |previous_boundary_block| OpenBatchRequestedWithdrawals {
        previous_boundary_block,
        requested_withdrawals: Vec::new(),
    };

    if parent_block_number == 0 {
        return Ok(empty_batch(0));
    }

    let previous_boundary = find_previous_batch_finalized_block(provider, parent_block_number)?;
    let from = previous_boundary.saturating_add(1);
    if from > parent_block_number {
        return Ok(empty_batch(previous_boundary));
    }

    let mut requested = Vec::new();
    for block_number in from..=parent_block_number {
        collect_requested_withdrawals_from_canonical_block(provider, block_number, &mut requested)?;
    }

    Ok(OpenBatchRequestedWithdrawals {
        previous_boundary_block: previous_boundary,
        requested_withdrawals: requested,
    })
}

fn withdrawal_batch_interval_elapsed<Provider>(
    provider: &Provider,
    previous_boundary_block: u64,
    parent_header: &TempoHeader,
    current_timestamp: u64,
    withdrawal_batch_interval: Duration,
) -> Result<bool, PayloadBuilderError>
where
    Provider: HeaderProvider<Header = TempoHeader>,
{
    let previous_boundary_timestamp = if previous_boundary_block == parent_header.number() {
        parent_header.timestamp()
    } else {
        provider
            .header_by_number(previous_boundary_block)
            .map_err(|err| PayloadBuilderError::Internal(err.into()))?
            .ok_or_else(|| {
                PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
                    "missing header for canonical zone block {previous_boundary_block}"
                )))
            })?
            .timestamp()
    };

    Ok(current_timestamp
        >= previous_boundary_timestamp.saturating_add(withdrawal_batch_interval.as_secs()))
}

/// Walk backwards from the `parent_block_number` and find the most recent
/// canonical block that emitted a successful `BatchFinalized` log.
fn find_previous_batch_finalized_block<Provider>(
    provider: &Provider,
    parent_block_number: u64,
) -> Result<u64, PayloadBuilderError>
where
    Provider: ReceiptProvider<Receipt = TempoReceipt>,
{
    for block_number in (1..=parent_block_number).rev() {
        let receipts = provider
            .receipts_by_block(BlockHashOrNumber::Number(block_number))
            .map_err(|err| PayloadBuilderError::Internal(err.into()))?
            .ok_or_else(|| {
                PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
                    "missing receipts for canonical zone block {block_number}"
                )))
            })?;

        if receipts.iter().any(|receipt| {
            if !receipt.status() {
                return false;
            }

            receipt.logs().iter().any(|log| {
                log.address == ZONE_OUTBOX_ADDRESS
                    && log.topics().first().copied().is_some_and(|topic| {
                        topic == abi::ZoneOutbox::BatchFinalized::SIGNATURE_HASH
                    })
            })
        }) {
            return Ok(block_number);
        }
    }

    Ok(0)
}

/// Collect successful `WithdrawalRequested` logs from one canonical block.
///
/// Loads the block's receipts and transactions, pairs each receipt with its
/// transaction hash, and appends any successful `WithdrawalRequested` events to
/// `requested_withdrawals` in transaction/log order.
fn collect_requested_withdrawals_from_canonical_block<Provider>(
    provider: &Provider,
    block_number: u64,
    requested_withdrawals: &mut Vec<RequestedWithdrawalContext>,
) -> Result<(), PayloadBuilderError>
where
    Provider: ReceiptProvider<Receipt = TempoReceipt>
        + TransactionsProvider<Transaction = TempoTxEnvelope>,
{
    let receipts = provider
        .receipts_by_block(BlockHashOrNumber::Number(block_number))
        .map_err(|err| PayloadBuilderError::Internal(err.into()))?
        .ok_or_else(|| {
            PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
                "missing receipts for canonical zone block {block_number}"
            )))
        })?;
    let transactions = provider
        .transactions_by_block(BlockHashOrNumber::Number(block_number))
        .map_err(|err| PayloadBuilderError::Internal(err.into()))?
        .ok_or_else(|| {
            PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
                "missing transactions for canonical zone block {block_number}"
            )))
        })?;

    if receipts.len() != transactions.len() {
        return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
            format!(
                "canonical zone block {block_number} has {} receipts for {} transactions",
                receipts.len(),
                transactions.len()
            ),
        )));
    }

    for (receipt, tx) in receipts.iter().zip(transactions.iter()) {
        collect_requested_withdrawals(receipt, *tx.tx_hash(), requested_withdrawals)?;
    }

    Ok(())
}

fn collect_requested_withdrawals(
    receipt: &tempo_primitives::TempoReceipt,
    tx_hash: B256,
    requested_withdrawals: &mut Vec<RequestedWithdrawalContext>,
) -> Result<(), PayloadBuilderError> {
    // Zone execution preserves reverted logs in receipts for observability, but
    // reverted `requestWithdrawal` calls roll back the outbox's pending storage.
    // Only successful receipts should contribute to end-of-block finalization.
    if !receipt.status() {
        return Ok(());
    }

    for log in receipt.logs() {
        if log.address != ZONE_OUTBOX_ADDRESS {
            continue;
        }

        if log
            .topics()
            .first()
            .copied()
            .is_some_and(|topic| topic == abi::ZoneOutbox::WithdrawalRequested::SIGNATURE_HASH)
        {
            let event = abi::ZoneOutbox::WithdrawalRequested::decode_log(log).map_err(|err| {
                PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
                    "failed to decode WithdrawalRequested log: {err}"
                )))
            })?;
            requested_withdrawals.push(RequestedWithdrawalContext {
                event: event.data,
                tx_hash,
            });
        }
    }

    Ok(())
}

/// Build the `advanceTempo(header, deposits, decryptions, enabledTokens)` system transaction.
///
/// This must be called **once per L1 block** at the start of a zone block (before user txs).
/// It calls [`ZoneInbox.advanceTempo`](crate::abi::ZoneInbox) which atomically:
/// - Advances the zone's view of Tempo by processing the L1 block header
/// - Enables newly-bridged TIP-20 tokens via the zone's TIP20Factory precompile
/// - Processes deposits from the queue (minting zone tokens to recipients)
/// - Validates the deposit hash chain against Tempo state
///
/// Takes a [`PreparedL1Block`] where all ECIES decryption, TIP-403 policy checks,
/// and ABI encoding have already been performed.
pub fn build_advance_tempo_tx(prepared: &PreparedL1Block) -> Recovered<TempoTxEnvelope> {
    // RLP-encode the Tempo header
    let mut header_rlp = Vec::new();
    prepared.header.header().encode(&mut header_rlp);

    let calldata = abi::ZoneInbox::advanceTempoCall {
        header: Bytes::from(header_rlp),
        deposits: prepared.queued_deposits.clone(),
        decryptions: prepared.decryptions.clone(),
        enabledTokens: prepared.enabled_tokens.clone(),
    }
    .abi_encode();

    let tx = TxLegacy {
        chain_id: None,
        nonce: 0,
        gas_price: 0,
        gas_limit: 0,
        to: ZONE_INBOX_ADDRESS.into(),
        value: U256::ZERO,
        input: calldata.into(),
    };

    Recovered::new_unchecked(
        TempoTxEnvelope::Legacy(Signed::new_unhashed(tx, TEMPO_SYSTEM_TX_SIGNATURE)),
        TEMPO_SYSTEM_TX_SENDER,
    )
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::{B256, Bytes, Log, U256, address};
    use alloy_sol_types::{SolCall, SolEvent};
    use reth_primitives_traits::SealedHeader;
    use tempo_primitives::{TempoHeader, TempoReceipt, TempoTxType};

    use crate::abi::{self, DepositType, ZoneInbox};
    use zone_l1::PreparedL1Block;

    fn make_withdrawal_requested_log(sender: alloy_primitives::Address) -> Log {
        let event = abi::ZoneOutbox::WithdrawalRequested {
            withdrawalIndex: 1,
            sender,
            token: address!("0x0000000000000000000000000000000000001000"),
            to: address!("0x0000000000000000000000000000000000002000"),
            amount: 500_000,
            fee: 0,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackRecipient: sender,
            data: Bytes::new(),
            revealTo: Bytes::new(),
        };

        Log {
            address: super::ZONE_OUTBOX_ADDRESS,
            data: event.encode_log_data(),
        }
    }

    fn make_receipt(success: bool, logs: Vec<Log>) -> TempoReceipt {
        TempoReceipt {
            tx_type: TempoTxType::Legacy,
            success,
            cumulative_gas_used: 21_000,
            logs,
        }
    }

    /// Verify that `build_advance_tempo_tx` constructs valid calldata for mixed
    /// deposit types. The calldata should include `QueuedDeposit` entries with the
    /// correct `DepositType` discriminator and `DecryptionData` for encrypted deposits.
    #[test]
    fn test_build_advance_tempo_tx_with_encrypted_deposit() {
        let token = address!("0x0000000000000000000000000000000000001000");
        let sender = address!("0x0000000000000000000000000000000000001234");
        let recipient = address!("0x0000000000000000000000000000000000005678");

        let header = TempoHeader {
            inner: Header {
                number: 1,
                ..Default::default()
            },
            ..Default::default()
        };

        // Build a PreparedL1Block directly — this test validates
        // `build_advance_tempo_tx` calldata encoding, not `prepare`.
        let prepared = PreparedL1Block {
            header: SealedHeader::seal_slow(header),
            queued_deposits: vec![
                abi::QueuedDeposit {
                    depositType: DepositType::Regular,
                    depositData: alloy_primitives::Bytes::from(
                        alloy_sol_types::SolValue::abi_encode(&abi::Deposit {
                            token,
                            sender,
                            to: recipient,
                            amount: 500_000,
                            bouncebackRecipient: recipient,
                            memo: B256::ZERO,
                        }),
                    ),
                    rejected: false,
                },
                abi::QueuedDeposit {
                    depositType: DepositType::Encrypted,
                    depositData: alloy_primitives::Bytes::from(
                        alloy_sol_types::SolValue::abi_encode(&abi::EncryptedDeposit {
                            token,
                            sender,
                            amount: 300_000,
                            bouncebackRecipient: sender,
                            keyIndex: U256::ZERO,
                            encrypted: abi::EncryptedDepositPayload {
                                ephemeralPubkeyX: B256::with_last_byte(0xDD),
                                ephemeralPubkeyYParity: 0x02,
                                ciphertext: vec![0xAA; 64].into(),
                                nonce: [0x05; 12].into(),
                                tag: [0x06; 16].into(),
                            },
                        }),
                    ),
                    rejected: false,
                },
            ],
            decryptions: vec![abi::DecryptionData {
                sharedSecret: B256::ZERO,
                sharedSecretYParity: 0x02,
                cpProof: abi::ChaumPedersenProof {
                    s: B256::ZERO,
                    c: B256::ZERO,
                },
            }],
            enabled_tokens: vec![],
        };

        let recovered_tx = super::build_advance_tempo_tx(&prepared);

        // Decode the calldata to verify structure.
        let envelope = recovered_tx.inner();
        let input = match envelope {
            tempo_primitives::TempoTxEnvelope::Legacy(signed) => &signed.tx().input,
            _ => panic!("expected Legacy tx"),
        };
        let decoded = ZoneInbox::advanceTempoCall::abi_decode(input)
            .expect("calldata should decode as advanceTempo");

        // Should have 2 queued deposits
        assert_eq!(decoded.deposits.len(), 2, "should have 2 queued deposits");

        // First should be Regular
        assert_eq!(
            decoded.deposits[0].depositType,
            DepositType::Regular,
            "first deposit should be Regular"
        );

        // Second should be Encrypted
        assert_eq!(
            decoded.deposits[1].depositType,
            DepositType::Encrypted,
            "second deposit should be Encrypted"
        );

        // Should have exactly 1 DecryptionData (one per encrypted deposit)
        assert_eq!(
            decoded.decryptions.len(),
            1,
            "should have 1 DecryptionData for the encrypted deposit"
        );
    }

    #[test]
    fn collect_requested_withdrawals_ignores_reverted_receipts() {
        let sender = address!("0x0000000000000000000000000000000000001234");
        let tx_hash = B256::with_last_byte(0x42);
        let mut requested = Vec::new();

        super::collect_requested_withdrawals(
            &make_receipt(false, vec![make_withdrawal_requested_log(sender)]),
            tx_hash,
            &mut requested,
        )
        .expect("reverted receipt should be ignored");
        assert!(
            requested.is_empty(),
            "reverted receipts must not add phantom withdrawals"
        );

        super::collect_requested_withdrawals(
            &make_receipt(true, vec![make_withdrawal_requested_log(sender)]),
            tx_hash,
            &mut requested,
        )
        .expect("successful receipt should decode");
        assert_eq!(requested.len(), 1, "successful receipt should be collected");
        assert_eq!(requested[0].tx_hash, tx_hash);
        assert_eq!(requested[0].event.sender, sender);
    }
}
