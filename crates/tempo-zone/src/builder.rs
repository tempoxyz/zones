//! Zone payload builder.
//!
//! Builds zone blocks by executing `advanceTempo` system transactions (one per L1 block)
//! followed by pool transactions and a withdrawal batch finalization.

use alloy_primitives::{Address, U256};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_errors::ProviderError;
use reth_evm::{
    ConfigureEvm, Database, NextBlockEnvAttributes,
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_node_api::FullNodeTypes;
use reth_node_builder::{BuilderContext, components::PayloadBuilderBuilder};
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderError};
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::AlloyBlockHeader as _;
use reth_revm::{State, database::StateProviderDatabase};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, TransactionPool,
    error::InvalidPoolTransactionError,
};
use std::{sync::Arc, time::Instant};
use tempo_chainspec::spec::TempoChainSpec;
use tempo_consensus::{TEMPO_GENERAL_GAS_DIVISOR, TEMPO_SHARED_GAS_DIVISOR};
use tempo_evm::TempoNextBlockEnvAttributes;
use tracing::{debug, error, info, warn};
use crate::evm::ZoneEvmConfig;
use crate::witness::{WitnessGenerator, WitnessGeneratorConfig};
use tempo_payload_types::TempoPayloadBuilderAttributes;
use tempo_primitives::{TempoHeader, TempoPrimitives};
use tempo_transaction_pool::TempoTransactionPool;

use super::node::ZoneNode;

/// Factory for constructing the zone payload builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZonePayloadFactory {
    deposit_queue: crate::DepositQueue,
    sequencer: Option<Address>,
    witness_store: crate::witness::SharedWitnessStore,
}

impl ZonePayloadFactory {
    pub fn new(
        deposit_queue: crate::DepositQueue,
        sequencer: Option<Address>,
        witness_store: crate::witness::SharedWitnessStore,
    ) -> Self {
        Self {
            deposit_queue,
            sequencer,
            witness_store,
        }
    }
}

impl<Node> PayloadBuilderBuilder<Node, TempoTransactionPool<Node::Provider>, ZoneEvmConfig>
    for ZonePayloadFactory
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type PayloadBuilder = ZonePayloadBuilder<Node::Provider>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: TempoTransactionPool<Node::Provider>,
        evm_config: ZoneEvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(ZonePayloadBuilder {
            pool,
            provider: ctx.provider().clone(),
            evm_config,
            deposit_queue: self.deposit_queue,
            sequencer: self.sequencer,
            witness_store: self.witness_store,
        })
    }
}
/// Zone payload builder that executes `advanceTempo` system txs + pool txs.
#[derive(Debug, Clone)]
pub struct ZonePayloadBuilder<Provider> {
    pool: TempoTransactionPool<Provider>,
    provider: Provider,
    evm_config: ZoneEvmConfig,
    deposit_queue: crate::DepositQueue,
    sequencer: Option<Address>,
    witness_store: crate::witness::SharedWitnessStore,
}

impl<Provider> ZonePayloadBuilder<Provider> {
    pub fn new(
        pool: TempoTransactionPool<Provider>,
        provider: Provider,
        evm_config: ZoneEvmConfig,
        deposit_queue: crate::DepositQueue,
        sequencer: Option<Address>,
        witness_store: crate::witness::SharedWitnessStore,
    ) -> Self {
        Self {
            pool,
            provider,
            evm_config,
            deposit_queue,
            sequencer,
            witness_store,
        }
    }
}

impl<Provider> ZonePayloadBuilder<Provider>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = TempoChainSpec> + Clone,
{
    /// Generate the zone state witness and store it for the zone monitor.
    ///
    /// Called after block execution while the parent state provider is still
    /// available for MPT proof generation. On failure (e.g., proof generation
    /// error), logs a warning but does not fail the block build — the block
    /// is still valid, just without a proof.
    fn generate_and_store_witness(
        &self,
        parent_header: &reth_primitives_traits::SealedHeader<TempoHeader>,
        sealed_block: &Arc<reth_primitives_traits::SealedBlock<tempo_primitives::Block>>,
        recorded_accesses: &crate::witness::RecordedAccesses,
        l1_reads: Vec<crate::witness::RecordedL1Read>,
        l1_block: &crate::l1::L1BlockDeposits,
        header_rlp: Vec<u8>,
    ) {
        use alloy_consensus::BlockHeader;
        use alloy_primitives::Bytes;
        use alloy_sol_types::SolValue;
        use zone_prover::types::{DepositType, QueuedDeposit, ZoneBlock, ZoneHeader};

        let block_number = sealed_block.number();

        // Open a fresh state provider for the parent state (avoids lifetime
        // entanglement with the execution database).
        let witness_sp = match self.provider.state_by_block_hash(parent_header.hash()) {
            Ok(sp) => sp,
            Err(e) => {
                warn!(
                    target: "zone::witness",
                    block_number, error = %e,
                    "Failed to open state provider for witness generation"
                );
                return;
            }
        };

        let state_root = parent_header.state_root();
        let accessed_accounts = recorded_accesses.accessed_accounts();
        let accessed_storage = recorded_accesses.accessed_storage();

        let sequencer = self.sequencer.unwrap_or_default();
        let witness_gen = WitnessGenerator::new(WitnessGeneratorConfig { sequencer });

        let zone_state_witness = match witness_gen.generate_zone_state_witness(
            &*witness_sp,
            state_root,
            &accessed_accounts,
            &accessed_storage,
        ) {
            Ok(w) => w,
            Err(e) => {
                warn!(
                    target: "zone::witness",
                    block_number, error = %e,
                    "Failed to generate zone state witness"
                );
                return;
            }
        };

        // Extract user transactions (skip advanceTempo at index 0 and
        // finalizeWithdrawalBatch at the end).
        let all_txs: Vec<_> = sealed_block.body().transactions().collect();
        let user_tx_bytes: Vec<Vec<u8>> = if all_txs.len() >= 2 {
            all_txs[1..all_txs.len() - 1]
                .iter()
                .map(|tx| alloy_rlp::encode(*tx))
                .collect()
        } else {
            vec![]
        };

        // Convert deposits from the L1 block to the prover's QueuedDeposit format.
        let deposits: Vec<QueuedDeposit> = l1_block
            .deposits
            .iter()
            .map(|d| {
                let deposit = crate::abi::Deposit {
                    sender: d.sender,
                    to: d.to,
                    amount: d.amount,
                    memo: d.memo,
                };
                QueuedDeposit {
                    deposit_type: DepositType::Regular,
                    deposit_data: Bytes::from(deposit.abi_encode()),
                }
            })
            .collect();

        let zone_block = ZoneBlock {
            number: block_number,
            parent_hash: parent_header.hash(),
            timestamp: sealed_block.timestamp(),
            beneficiary: sealed_block.beneficiary(),
            expected_state_root: sealed_block.state_root(),
            tempo_header_rlp: Some(header_rlp.clone()),
            deposits,
            decryptions: vec![],
            finalize_withdrawal_batch_count: Some(U256::MAX),
            transactions: user_tx_bytes,
        };

        let prev_block_header = ZoneHeader {
            parent_hash: parent_header.parent_hash(),
            beneficiary: parent_header.beneficiary(),
            state_root,
            transactions_root: parent_header.transactions_root(),
            receipts_root: parent_header.receipts_root(),
            number: parent_header.number(),
            timestamp: parent_header.timestamp(),
        };

        let chain_id = {
            use reth_chainspec::EthChainSpec;
            self.provider.chain_spec().chain().id()
        };

        let witness = crate::witness::BuiltBlockWitness {
            zone_block,
            zone_state_witness,
            prev_block_header,
            l1_reads,
            chain_id,
            tempo_header_rlp: header_rlp,
        };

        let mut store = self.witness_store.lock().expect("witness store poisoned");
        store.insert(block_number, witness);

        info!(
            target: "zone::witness",
            block_number,
            accounts = accessed_accounts.len(),
            storage_accounts = accessed_storage.len(),
            "Stored witness data for prover"
        );
    }
}

impl<Provider> PayloadBuilder for ZonePayloadBuilder<Provider>
where
    Provider:
        StateProviderFactory + ChainSpecProvider<ChainSpec = TempoChainSpec> + Clone + 'static,
{
    type Attributes = TempoPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload<TempoPrimitives>;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        let BuildArguments {
            mut cached_reads,
            config,
            cancel,
            best_payload: _,
        } = args;
        let PayloadConfig {
            parent_header,
            attributes,
        } = config;

        let start = Instant::now();

        // Read the current tempoBlockHash and tempoBlockNumber from TempoState storage
        // to validate the next L1 block we process is the expected successor.
        let (tempo_block_hash, expected_l1_number) = {
            let sp = self.provider.state_by_block_hash(parent_header.hash())?;
            let hash = sp
                .storage(
                    crate::abi::TEMPO_STATE_ADDRESS,
                    alloy_primitives::B256::ZERO,
                )
                .map_err(|e| PayloadBuilderError::Internal(e.into()))?
                .map(|v| alloy_primitives::B256::from(v.to_be_bytes()))
                .unwrap_or_default();
            // tempoBlockNumber is at slot 7, offset 0 (packed as lowest uint64 in the slot,
            // alongside tempoGasLimit, tempoGasUsed, tempoTimestamp)
            let slot7 = sp
                .storage(crate::abi::TEMPO_STATE_ADDRESS, U256::from(7).into())
                .map_err(|e| PayloadBuilderError::Internal(e.into()))?
                .unwrap_or_default();
            // Extract lowest 8 bytes (uint64 at offset 0)
            let tempo_block_number: u64 = (slot7 & U256::from(u64::MAX)).to::<u64>();
            let expected: u64 = tempo_block_number + 1;
            (hash, expected)
        };

        info!(
            target: "zone::payload",
            %tempo_block_hash,
            expected_l1_number,
            "TempoState current state"
        );

        // Take exactly one L1 block per zone block — advanceTempo advances Tempo state
        // by exactly one block, maintaining sequential chain continuity.
        // The ZoneEngine ensures an L1 block is queued before triggering a build.
        let l1_block = match self
            .deposit_queue
            .lock()
            .expect("deposit queue poisoned")
            .pop_next()
        {
            Some(block) => block,
            None => {
                debug!(target: "zone::payload", "No L1 block available, cancelling build");
                return Ok(BuildOutcome::Cancelled);
            }
        };

        // Validate chain continuity: the L1 block must be exactly tempoBlockNumber + 1
        // and its parent hash must match the stored tempoBlockHash.
        if l1_block.header.inner.number != expected_l1_number {
            error!(
                target: "zone::payload",
                got = l1_block.header.inner.number,
                expected = expected_l1_number,
                "L1 block number mismatch — chain continuity broken"
            );
            return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
                format!(
                    "L1 block number mismatch: got {} expected {}",
                    l1_block.header.inner.number, expected_l1_number
                ),
            )));
        }
        if l1_block.header.inner.parent_hash != tempo_block_hash {
            error!(
                target: "zone::payload",
                got = %l1_block.header.inner.parent_hash,
                expected = %tempo_block_hash,
                l1_block = l1_block.header.inner.number,
                "L1 parent hash mismatch — chain continuity broken"
            );
            return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
                format!(
                    "L1 parent hash mismatch at block {}: got {} expected {}",
                    l1_block.header.inner.number,
                    l1_block.header.inner.parent_hash,
                    tempo_block_hash
                ),
            )));
        }

        let total_deposits = l1_block.deposits.len();

        info!(
            target: "zone::payload",
            l1_block = l1_block.header.inner.number,
            deposits = total_deposits,
            "Including advanceTempo system tx (chain continuity OK)"
        );
        for deposit in &l1_block.deposits {
            debug!(
                target: "zone::payload",
                sender = %deposit.sender,
                to = %deposit.to,
                amount = %deposit.amount,
                l1_block = l1_block.header.inner.number,
                "Deposit -> advanceTempo"
            );
        }

        let state_provider = self.provider.state_by_block_hash(parent_header.hash())?;
        let state_provider: Box<dyn StateProvider> = state_provider;
        let state = StateProviderDatabase::new(&state_provider);

        // Wrap the database in a RecordingDatabase to capture all state accesses
        // for witness generation. The `accesses` handle is cloned so we can
        // retrieve recorded data after the State is consumed.
        let recorded_accesses = crate::witness::RecordedAccesses::new();
        let recording_db = crate::witness::RecordingDatabase::new(
            Box::new(cached_reads.as_db_mut(state)) as Box<dyn Database<Error = ProviderError>>,
            recorded_accesses.clone(),
        );
        let mut db = State::builder()
            .with_database(recording_db)
            .with_bundle_update()
            .build();

        let chain_spec = self.provider.chain_spec();

        let block_gas_limit = parent_header.gas_limit();
        let shared_gas_limit = block_gas_limit / TEMPO_SHARED_GAS_DIVISOR;
        let non_shared_gas_limit = block_gas_limit - shared_gas_limit;
        let general_gas_limit = non_shared_gas_limit / TEMPO_GENERAL_GAS_DIVISOR;

        let mut cumulative_gas_used = 0u64;
        let total_fees = U256::ZERO;

        let mut builder = self
            .evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                TempoNextBlockEnvAttributes {
                    inner: NextBlockEnvAttributes {
                        timestamp: attributes.timestamp(),
                        suggested_fee_recipient: attributes.suggested_fee_recipient(),
                        prev_randao: attributes.prev_randao(),
                        gas_limit: block_gas_limit,
                        parent_beacon_block_root: attributes.parent_beacon_block_root(),
                        withdrawals: Some(attributes.withdrawals().clone()),
                        extra_data: attributes.extra_data().clone(),
                    },
                    general_gas_limit,
                    shared_gas_limit,
                    timestamp_millis_part: attributes.timestamp_millis_part(),
                    subblock_fee_recipients: Default::default(),
                },
            )
            .map_err(PayloadBuilderError::other)?;

        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(%err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;

        // Set the L1 recording block index for this zone block.
        // Block index is 0-based within a batch; for single-block-per-build this is always 0.
        self.evm_config.set_l1_recording_block_index(0);

        // Execute advanceTempo system transaction — exactly one per zone block.
        let header_rlp = alloy_rlp::encode(&l1_block.header);
        {
            info!(
                target: "zone::payload",
                l1_block_number = l1_block.header.inner.number,
                l1_parent_hash = %l1_block.header.inner.parent_hash,
                l1_block_hash = %alloy_primitives::keccak256(&header_rlp),
                header_rlp_len = header_rlp.len(),
                "advanceTempo header details"
            );

            let advance_tx =
                crate::system_tx::build_advance_tempo_tx(&l1_block.header, &l1_block.deposits);
            if let Err(err) = builder.execute_transaction(advance_tx) {
                error!(
                    ?err,
                    l1_block = l1_block.header.inner.number,
                    deposits = l1_block.deposits.len(),
                    "advanceTempo system tx failed"
                );
                return Err(PayloadBuilderError::evm(err));
            }
        }

        // Execute pool transactions
        // TODO: Use gas accounting from TempoPayloadBuilder (payment vs non-payment limits, etc.)
        let base_fee = builder.evm_mut().block.basefee;
        let mut best_txs = self
            .pool
            .best_transactions_with_attributes(BestTransactionsAttributes::new(base_fee, None));

        while let Some(pool_tx) = best_txs.next() {
            if cumulative_gas_used + pool_tx.gas_limit() > non_shared_gas_limit {
                best_txs.mark_invalid(
                    &pool_tx,
                    &InvalidPoolTransactionError::ExceedsGasLimit(
                        pool_tx.gas_limit(),
                        non_shared_gas_limit - cumulative_gas_used,
                    ),
                );
                continue;
            }

            if cancel.is_cancelled() {
                return Ok(BuildOutcome::Cancelled);
            }

            let tx_with_env = pool_tx.transaction.clone().into_with_tx_env();
            match builder.execute_transaction(tx_with_env) {
                Ok(gas_used) => {
                    cumulative_gas_used += gas_used;
                }
                Err(reth_evm::block::BlockExecutionError::Validation(
                    reth_evm::block::BlockValidationError::InvalidTx { error, .. },
                )) => {
                    if !error.is_nonce_too_low() {
                        best_txs.mark_invalid(
                            &pool_tx,
                            &InvalidPoolTransactionError::Consensus(
                                reth_primitives_traits::transaction::error::InvalidTransactionError::TxTypeNotSupported,
                            ),
                        );
                    }
                    continue;
                }
                Err(err) => return Err(PayloadBuilderError::evm(err)),
            }
        }

        // Finalize the withdrawal batch — must run after all user txs.
        // Calls ZoneOutbox.finalizeWithdrawalBatch(MAX, blockNumber) to build the
        // withdrawal hash chain and write batch state for proof generation.
        let block_number: u64 = builder
            .evm_mut()
            .block
            .number
            .try_into()
            .expect("block number fits u64");
        let finalize_tx =
            crate::system_tx::build_finalize_withdrawal_batch_tx(U256::MAX, block_number);
        if let Err(err) = builder.execute_transaction(finalize_tx) {
            error!(?err, "finalizeWithdrawalBatch system tx failed");
            return Err(PayloadBuilderError::evm(err));
        }

        let BlockBuilderOutcome {
            execution_result,
            block,
            ..
        } = builder.finish(&state_provider)?;

        let requests = chain_spec
            .is_prague_active_at_timestamp(attributes.timestamp())
            .then_some(execution_result.requests);

        let sealed_block = Arc::new(block.sealed_block().clone());
        let elapsed = start.elapsed();

        // Collect recorded L1 reads from the precompile wrapper.
        let l1_reads = self.evm_config.take_l1_reads().unwrap_or_default();

        debug!(
            target: "zone::payload",
            accounts = recorded_accesses.accessed_accounts().len(),
            storage_accounts = recorded_accesses.accessed_storage().len(),
            l1_reads = l1_reads.len(),
            "Recorded state accesses for witness generation"
        );

        info!(
            number = sealed_block.number(),
            hash = ?sealed_block.hash(),
            gas_used = sealed_block.gas_used(),
            deposits = total_deposits,
            tx_count = sealed_block.body().transactions.len(),
            ?elapsed,
            "Built zone payload"
        );

        // Generate witness data for the prover while we still have state provider access.
        self.generate_and_store_witness(
            &parent_header,
            &sealed_block,
            &recorded_accesses,
            l1_reads,
            &l1_block,
            header_rlp,
        );

        let payload =
            EthBuiltPayload::new(attributes.payload_id(), sealed_block, total_fees, requests);

        drop(db);
        Ok(BuildOutcome::Better {
            payload,
            cached_reads,
        })
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
            config,
            Default::default(),
            Default::default(),
        ))?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}
