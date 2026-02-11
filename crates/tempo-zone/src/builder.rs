//! Zone payload builder.
//!
//! Simple payload builder that executes deposit mint system transactions
//! from L1 and pool transactions.

use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, sol};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_errors::ProviderError;
use reth_evm::{
    ConfigureEvm, Database, NextBlockEnvAttributes,
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_node_api::FullNodeTypes;
use reth_node_builder::{BuilderContext, components::PayloadBuilderBuilder};
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderError};
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::{AlloyBlockHeader as _, Recovered};
use reth_revm::{State, database::StateProviderDatabase};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_tracing::tracing::{debug, error, info, warn};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, TransactionPool,
    error::InvalidPoolTransactionError,
};
use std::{sync::Arc, time::Instant};
use tempo_chainspec::spec::TempoChainSpec;
use tempo_consensus::{TEMPO_GENERAL_GAS_DIVISOR, TEMPO_SHARED_GAS_DIVISOR};
use tempo_evm::{TempoEvmConfig, TempoNextBlockEnvAttributes};
use tempo_payload_types::TempoPayloadBuilderAttributes;
use tempo_primitives::{
    TempoHeader, TempoPrimitives, TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};
use tempo_transaction_pool::TempoTransactionPool;

use crate::l1::Deposit;

use super::node::ZoneNode;

sol! {
    function mint(address to, uint256 amount);
}

/// Factory for constructing the zone payload builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZonePayloadFactory {
    deposit_queue: crate::DepositQueue,
    token_address: Address,
}

impl ZonePayloadFactory {
    pub fn new(deposit_queue: crate::DepositQueue, token_address: Address) -> Self {
        Self {
            deposit_queue,
            token_address,
        }
    }
}

impl<Node> PayloadBuilderBuilder<Node, TempoTransactionPool<Node::Provider>, TempoEvmConfig>
    for ZonePayloadFactory
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type PayloadBuilder = ZonePayloadBuilder<Node::Provider>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: TempoTransactionPool<Node::Provider>,
        evm_config: TempoEvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(ZonePayloadBuilder {
            pool,
            provider: ctx.provider().clone(),
            evm_config,
            deposit_queue: self.deposit_queue,
            token_address: self.token_address,
        })
    }
}

/// Simple zone payload builder that executes deposit mint txs + pool txs.
///
/// TODO: Integrate with TempoPayloadBuilder for shared metrics, subblock support, etc.
#[derive(Debug, Clone)]
pub struct ZonePayloadBuilder<Provider> {
    pool: TempoTransactionPool<Provider>,
    provider: Provider,
    evm_config: TempoEvmConfig,
    deposit_queue: crate::DepositQueue,
    token_address: Address,
}

impl<Provider> ZonePayloadBuilder<Provider>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = TempoChainSpec>,
{
    fn build_deposit_mint_txs(&self, deposits: &[Deposit]) -> Vec<Recovered<TempoTxEnvelope>> {
        let chain_id = Some(self.provider.chain_spec().chain().id());

        deposits
            .iter()
            .map(|deposit| {
                let calldata = mintCall {
                    to: deposit.to,
                    amount: U256::from(deposit.amount),
                }
                .abi_encode();

                Recovered::new_unchecked(
                    TempoTxEnvelope::Legacy(Signed::new_unhashed(
                        TxLegacy {
                            chain_id,
                            nonce: 0,
                            gas_price: 0,
                            gas_limit: 0,
                            to: self.token_address.into(),
                            value: U256::ZERO,
                            input: calldata.into(),
                        },
                        TEMPO_SYSTEM_TX_SIGNATURE,
                    )),
                    TEMPO_SYSTEM_TX_SENDER,
                )
            })
            .collect()
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

        let pending_deposits = self.deposit_queue.drain();

        if !pending_deposits.is_empty() {
            info!(
                target: "zone::payload",
                count = pending_deposits.len(),
                "Including deposit mint txs in block"
            );
            for deposit in &pending_deposits {
                debug!(
                    target: "zone::payload",
                    sender = %deposit.sender,
                    to = %deposit.to,
                    amount = %deposit.amount,
                    l1_block = deposit.l1_block_number,
                    "Deposit -> mint"
                );
            }
        }

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

        // Execute deposit mint system transactions
        let deposit_txs = self.build_deposit_mint_txs(&pending_deposits);
        for tx in deposit_txs {
            if let Err(err) = builder.execute_transaction(tx) {
                error!(?err, "deposit mint system tx failed");
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

        info!(
            number = sealed_block.number(),
            hash = ?sealed_block.hash(),
            gas_used = sealed_block.gas_used(),
            deposits = pending_deposits.len(),
            tx_count = sealed_block.body().transactions.len(),
            ?elapsed,
            "Built zone payload"
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
