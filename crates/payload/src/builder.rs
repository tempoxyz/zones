//! Zone payload builder.
//!
//! Builds zone blocks by executing `advanceTempo` system transactions (one per L1 block)
//! followed by pool transactions and a withdrawal batch finalization.

use crate::{
    WithdrawalRevealEncryptor,
    abi::{self, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS},
};
use alloy_consensus::{Signed, TxLegacy};
use alloy_eips::eip4895::Withdrawals;
use alloy_evm::{
    Evm, EvmFactory,
    block::{BlockExecutorFactory, CommitChanges, TxResult},
    revm::context_interface::block::Block as RevmBlock,
};
use alloy_primitives::{Bytes, U256};
use alloy_rlp::Encodable;
use alloy_sol_types::SolCall;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_errors::ProviderError;
use reth_evm::{
    BlockEnvFor, ConfigureEvm, Database, NextBlockEnvAttributes,
    execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionOutput},
};
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{BuilderContext, components::PayloadBuilderBuilder};
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderError};
use reth_payload_primitives::{BuiltPayloadExecutedBlock, PayloadAttributes};
use reth_primitives_traits::{AlloyBlockHeader as _, Recovered};
use reth_revm::{State, cancelled::CancelOnDrop, database::StateProviderDatabase};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, TransactionPool, ValidPoolTransaction,
    error::InvalidPoolTransactionError,
};
use std::{sync::Arc, time::Instant};
use tempo_evm::TempoNextBlockEnvAttributes;
use tempo_payload_types::{EncodedBlock, TempoBuiltPayload};
use tempo_primitives::{
    TempoHeader, TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};
use tempo_transaction_pool::{TempoTransactionPool, transaction::TempoPooledTransaction};
use tracing::{error, info, warn};
use zone_chainspec::ZoneChainSpec;
use zone_l1::{PreparedL1Block, TempoStateExt};

use crate::{ZonePayloadAttributes, ZonePayloadTypes};

/// Default empty-batch cadence: every 120 zone blocks (~60 sec at Tempo's 500 ms block time).
pub const DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS: u64 = 120;

/// Factory for constructing the zone payload builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZonePayloadFactory {
    withdrawal_batch_interval_blocks: u64,
    withdrawal_reveal_encryptor: Option<Arc<dyn WithdrawalRevealEncryptor>>,
}

impl ZonePayloadFactory {
    /// Create a factory that finalizes empty withdrawal batches every `interval_blocks` zone blocks.
    pub fn new(withdrawal_batch_interval_blocks: u64) -> Self {
        Self {
            withdrawal_batch_interval_blocks: withdrawal_batch_interval_blocks.max(1),
            withdrawal_reveal_encryptor: None,
        }
    }

    pub fn with_withdrawal_reveal_encryptor(
        mut self,
        encryptor: Arc<dyn WithdrawalRevealEncryptor>,
    ) -> Self {
        self.withdrawal_reveal_encryptor = Some(encryptor);
        self
    }
}

impl Default for ZonePayloadFactory {
    fn default() -> Self {
        Self::new(DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS)
    }
}

impl<Node, EvmConfig> PayloadBuilderBuilder<Node, TempoTransactionPool<Node::Provider>, EvmConfig>
    for ZonePayloadFactory
where
    Node: FullNodeTypes,
    Node::Types: NodeTypes<
            Primitives = tempo_primitives::TempoPrimitives,
            ChainSpec = ZoneChainSpec,
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
            withdrawal_batch_interval_blocks: self.withdrawal_batch_interval_blocks,
            withdrawal_reveal_encryptor: self.withdrawal_reveal_encryptor.clone(),
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
    /// Number of zone blocks between withdrawal batch boundaries.
    withdrawal_batch_interval_blocks: u64,
    /// Encrypts authenticated-withdrawal sender reveal data for batch finalization.
    withdrawal_reveal_encryptor: Option<Arc<dyn WithdrawalRevealEncryptor>>,
}

impl<Provider, EvmConfig> PayloadBuilder for ZonePayloadBuilder<Provider, EvmConfig>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = ZoneChainSpec> + Clone + 'static,
    EvmConfig: ConfigureEvm<
            Primitives = tempo_primitives::TempoPrimitives,
            NextBlockEnvCtx = TempoNextBlockEnvAttributes,
        > + 'static,
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

        let state_provider = self.provider.state_by_block_hash(parent_header.hash())?;
        let prepared = attributes.l1_block();
        validate_l1_continuity(state_provider.as_ref(), prepared)?;

        let total_deposits = prepared.queued_deposits.len();

        info!(
            target: "zone::payload",
            zone_block = parent_header.number() + 1,
            l1_block = prepared.header.inner.number,
            deposits = total_deposits,
            enabled_tokens = prepared.enabled_tokens.len(),
            "Including advanceTempo system tx (chain continuity OK)"
        );

        let state = StateProviderDatabase::new(state_provider.as_ref());
        let mut db = State::builder()
            .with_database(
                Box::new(cached_reads.as_db_mut(state)) as Box<dyn Database<Error = ProviderError>>
            )
            .with_bundle_update()
            .build();

        let chain_spec = self.provider.chain_spec();

        let block_gas_limit = parent_header.gas_limit();

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
        let mut builder = self
            .evm_config
            .builder_for_next_block(&mut db, &parent_header, next_block_env_attributes)
            .map_err(PayloadBuilderError::other)?;
        let base_fee = builder.evm().block().basefee();
        let block_number: u64 = builder
            .evm()
            .block()
            .number()
            .try_into()
            .expect("block number fits u64");

        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(%err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;

        let pending_withdrawals_at_block_start =
            read_pending_withdrawals_from_outbox(&mut builder, block_number)?;
        let has_prior_withdrawals = !pending_withdrawals_at_block_start.is_empty();

        // Execute advanceTempo system transaction — exactly one per zone block.
        execute_required_system_transaction(&mut builder, build_advance_tempo_tx(prepared))
            .map_err(|err| {
                error!(
                    ?err,
                    l1_block = prepared.header.inner.number,
                    deposits = total_deposits,
                    "advanceTempo system tx failed"
                );
                err
            })?;

        // Execute pool transactions. The block executor owns gas-capacity accounting.
        let mut best_txs = self
            .pool
            .best_transactions_with_attributes(BestTransactionsAttributes::new(base_fee, None));
        if execute_pool_transactions(&mut builder, &mut best_txs, &cancel)?
            == PoolExecutionOutcome::Cancelled
        {
            return Ok(BuildOutcome::Cancelled);
        }

        finalize_withdrawal_batch_if_needed(
            &mut builder,
            block_number,
            self.withdrawal_batch_interval_blocks,
            has_prior_withdrawals,
            self.withdrawal_reveal_encryptor.as_deref(),
        )?;

        let BlockBuilderOutcome {
            execution_result,
            hashed_state,
            trie_updates,
            block,
            block_access_list: _,
        } = builder.finish(&*state_provider, None)?;

        zone_evm::validate_advance_tempo_transactions(&block.sealed_block().body().transactions)
            .map_err(PayloadBuilderError::other)?;

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
        let eth_payload = EthBuiltPayload::new(recovered_block.clone(), U256::ZERO, requests, None);

        let execution_output = BlockExecutionOutput {
            result: execution_result,
            state: db.take_bundle(),
        };

        let executed_block = BuiltPayloadExecutedBlock {
            recovered_block,
            execution_output: Arc::new(execution_output),
            hashed_state: Arc::new(hashed_state),
            trie_updates: Arc::new(trie_updates),
            changed_paths: None,
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

/// Validate that the prepared L1 block is the next block expected by TempoState.
fn validate_l1_continuity(
    state_provider: &dyn StateProvider,
    prepared: &PreparedL1Block,
) -> Result<(), PayloadBuilderError> {
    let stored_l1 = state_provider
        .tempo_num_hash()
        .map_err(|err| PayloadBuilderError::Internal(err.into()))?;
    let expected_block_number = stored_l1.number + 1;

    info!(
        target: "zone::payload",
        stored_l1_block_hash = %stored_l1.hash,
        expected_tempo_block_number = expected_block_number,
        "TempoState current state"
    );

    if prepared.header.inner.number != expected_block_number {
        error!(
            target: "zone::payload",
            got = prepared.header.inner.number,
            expected = expected_block_number,
            "L1 block number mismatch — chain continuity broken"
        );
        return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
            format!(
                "L1 block number mismatch: got {} expected {expected_block_number}",
                prepared.header.inner.number
            ),
        )));
    }

    if prepared.header.inner.parent_hash != stored_l1.hash {
        error!(
            target: "zone::payload",
            got = %prepared.header.inner.parent_hash,
            expected = %stored_l1.hash,
            l1_block = prepared.header.inner.number,
            "L1 parent hash mismatch — chain continuity broken"
        );
        return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
            format!(
                "L1 parent hash mismatch at block {}: got {} expected {}",
                prepared.header.inner.number, prepared.header.inner.parent_hash, stored_l1.hash
            ),
        )));
    }

    Ok(())
}

/// Execute the best pool transactions until the iterator is exhausted or the build is cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolExecutionOutcome {
    Complete,
    Cancelled,
}

fn execute_pool_transactions<B, T>(
    builder: &mut B,
    best_txs: &mut T,
    cancel: &CancelOnDrop,
) -> Result<PoolExecutionOutcome, PayloadBuilderError>
where
    B: BlockBuilder<Primitives = tempo_primitives::TempoPrimitives>,
    <B::Executor as reth_evm::execute::BlockExecutor>::Evm:
        alloy_evm::Evm<Tx = tempo_revm::TempoTxEnv>,
    T: BestTransactions<Item = Arc<ValidPoolTransaction<TempoPooledTransaction>>>,
{
    while let Some(pool_tx) = best_txs.next() {
        if cancel.is_cancelled() {
            return Ok(PoolExecutionOutcome::Cancelled);
        }

        let tx_with_env = pool_tx.transaction.clone().into_with_tx_env();
        match builder.execute_transaction(tx_with_env) {
            Ok(_) => {}
            Err(reth_evm::block::BlockExecutionError::Validation(
                reth_evm::block::BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit,
                    block_available_gas,
                },
            )) => {
                best_txs.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::ExceedsGasLimit(
                        transaction_gas_limit,
                        block_available_gas,
                    ),
                );
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
            }
            Err(reth_evm::block::BlockExecutionError::Internal(
                reth_evm::block::InternalBlockExecutionError::EVM { ref error, .. },
            )) if zone_precompiles::is_zone_rpc_error(&error.to_string()) => {
                warn!(target: "zone::payload", %error, ?pool_tx, "skipping pool tx due to transient RPC error");
            }
            Err(err) => return Err(PayloadBuilderError::evm(err)),
        }
    }

    Ok(PoolExecutionOutcome::Complete)
}

/// Finalize withdrawals when the block started with pending requests or reaches a batch boundary.
fn finalize_withdrawal_batch_if_needed<B>(
    builder: &mut B,
    block_number: u64,
    interval_blocks: u64,
    has_prior_withdrawals: bool,
    encryptor: Option<&dyn WithdrawalRevealEncryptor>,
) -> Result<(), PayloadBuilderError>
where
    B: BlockBuilder<Primitives = tempo_primitives::TempoPrimitives>,
{
    if !has_prior_withdrawals && !block_number.is_multiple_of(interval_blocks) {
        return Ok(());
    }

    let pending_withdrawals = read_pending_withdrawals_from_outbox(builder, block_number)?;
    let encrypted_senders = pending_withdrawals
        .iter()
        .map(|request| {
            if request.revealTo.is_empty() {
                Ok(Bytes::new())
            } else {
                let encryptor = encryptor.ok_or_else(|| {
                    PayloadBuilderError::Internal(reth_errors::RethError::msg(
                        "withdrawal reveal encryption requested but no encryptor is configured",
                    ))
                })?;
                encryptor
                    .encrypt_sender(request.revealTo.as_ref(), request.sender, request.txHash)
                    .map(Bytes::from)
                    .ok_or_else(|| {
                        PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
                            "failed to encrypt authenticated sender reveal for tx {}",
                            request.txHash
                        )))
                    })
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let count = U256::from(pending_withdrawals.len());
    let finalize_tx = build_finalize_withdrawal_batch_tx(count, block_number, encrypted_senders);
    execute_required_system_transaction(builder, finalize_tx).map_err(|err| {
        error!(
            ?err,
            block_number, "finalizeWithdrawalBatch system tx failed"
        );
        err
    })
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

/// Execute a required system transaction and fail the payload if the executor rejects it.
///
/// Tempo's EVM reports system-call reverts as transaction errors, so successful execution does
/// not need a second result-status check.
fn execute_required_system_transaction<B>(
    builder: &mut B,
    tx: Recovered<TempoTxEnvelope>,
) -> Result<(), PayloadBuilderError>
where
    B: BlockBuilder<Primitives = tempo_primitives::TempoPrimitives>,
{
    builder
        .execute_transaction(tx)
        .map(|_| ())
        .map_err(PayloadBuilderError::evm)
}

/// Read all pending withdrawals in the ZoneOutbox
fn read_pending_withdrawals_from_outbox<B>(
    builder: &mut B,
    block_number: u64,
) -> Result<Vec<abi::ZoneOutbox::PendingWithdrawal>, PayloadBuilderError>
where
    B: BlockBuilder<Primitives = tempo_primitives::TempoPrimitives>,
{
    let calldata = abi::ZoneOutbox::getPendingWithdrawalsCall {}.abi_encode();
    let output = execute_outbox_view_call(
        builder,
        calldata.into(),
        block_number,
        "getPendingWithdrawals",
    )?;

    abi::ZoneOutbox::getPendingWithdrawalsCall::abi_decode_returns(&output).map_err(|err| {
        PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
            "failed to decode getPendingWithdrawals return data: {err}"
        )))
    })
}

fn execute_outbox_view_call<B>(
    builder: &mut B,
    calldata: Bytes,
    block_number: u64,
    label: &str,
) -> Result<Bytes, PayloadBuilderError>
where
    B: BlockBuilder<Primitives = tempo_primitives::TempoPrimitives>,
{
    let tx = TxLegacy {
        chain_id: None,
        nonce: 0,
        gas_price: 0,
        // Tempo applies its fixed internal system-call limit and reports zero gas used.
        // Keeping the envelope limit at zero also lets this simulation run after a full block.
        gas_limit: 0,
        to: ZONE_OUTBOX_ADDRESS.into(),
        value: U256::ZERO,
        input: calldata,
    };
    let tx = Recovered::new_unchecked(
        TempoTxEnvelope::Legacy(Signed::new_unhashed(tx, TEMPO_SYSTEM_TX_SIGNATURE)),
        TEMPO_SYSTEM_TX_SENDER,
    );
    let mut output = None;

    match builder.execute_transaction_with_commit_condition(tx, |result| {
        let evm_result = result.result();
        output = Some(evm_result.result.output().cloned().unwrap_or_default());
        CommitChanges::No
    }) {
        Ok(_) => output.ok_or_else(|| {
            PayloadBuilderError::Internal(reth_errors::RethError::msg(format!(
                "ZoneOutbox {label} view returned no output at zone block {block_number}"
            )))
        }),
        Err(err) => {
            error!(?err, label, "ZoneOutbox view simulation failed");
            Err(PayloadBuilderError::evm(err))
        }
    }
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
    use alloy_primitives::{B256, U256, address};
    use alloy_sol_types::SolCall;
    use reth_primitives_traits::SealedHeader;
    use tempo_primitives::TempoHeader;

    use crate::abi::{self, DepositType, ZoneInbox};
    use zone_l1::PreparedL1Block;

    #[test]
    fn withdrawal_batch_cadence_is_deterministic_from_block_number() {
        let blocks = super::DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS;

        assert_eq!(blocks, 120);
        assert_ne!(119 % blocks, 0);
        assert_eq!(120 % blocks, 0);
        assert_eq!(240 % blocks, 0);
    }

    #[test]
    fn zero_batch_interval_finalizes_every_block() {
        assert_eq!(
            super::ZonePayloadFactory::new(0).withdrawal_batch_interval_blocks,
            1
        );
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
}
