//! Zone payload builder.
//!
//! Builds zone blocks by executing `advanceTempo` system transactions (one per L1 block)
//! followed by pool transactions and a withdrawal batch finalization.

use crate::{
    abi::{self, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS},
    evm::ZoneEvmConfig,
    ext::TempoStateExt,
    l1::PreparedL1Block,
    payload::ZonePayloadBuilderAttributes,
};
use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::{Bytes, U256};
use alloy_rlp::Encodable;
use alloy_sol_types::SolCall;
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
use reth_primitives_traits::{AlloyBlockHeader as _, Recovered};
use reth_revm::{State, database::StateProviderDatabase};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, TransactionPool,
    error::InvalidPoolTransactionError,
};
use std::{sync::Arc, time::Instant};
use tempo_chainspec::spec::TempoChainSpec;
use tempo_consensus::TEMPO_SHARED_GAS_DIVISOR;
use tempo_evm::TempoNextBlockEnvAttributes;
use tempo_primitives::{
    TempoHeader, TempoPrimitives, TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};
use tempo_transaction_pool::TempoTransactionPool;
use tracing::{error, info, warn};

use super::node::ZoneNode;

/// Factory for constructing the zone payload builder.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ZonePayloadFactory;

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
        })
    }
}

/// Zone payload builder that executes `advanceTempo` system txs + pool txs.
#[derive(Debug, Clone)]
pub struct ZonePayloadBuilder<Provider> {
    /// Transaction pool for selecting pool txs to include in the block.
    pool: TempoTransactionPool<Provider>,
    /// State provider for reading chain state during block building.
    provider: Provider,
    /// Zone-specific EVM configuration (precompiles, hardfork spec, gas params).
    evm_config: ZoneEvmConfig,
}

impl<Provider> PayloadBuilder for ZonePayloadBuilder<Provider>
where
    Provider:
        StateProviderFactory + ChainSpecProvider<ChainSpec = TempoChainSpec> + Clone + 'static,
{
    type Attributes = ZonePayloadBuilderAttributes;
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
        let shared_gas_limit = block_gas_limit / TEMPO_SHARED_GAS_DIVISOR;
        let general_gas_limit = 0;

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
                        extra_data: attributes.extra_data(),
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

        // Execute advanceTempo system transaction — exactly one per zone block.
        {
            let advance_tx = build_advance_tempo_tx(prepared);
            let mut reverted = false;
            match builder.execute_transaction_with_result_closure(advance_tx, |result| {
                if !result.is_success() {
                    let revert_data = result.output().cloned().unwrap_or_default();
                    error!(
                        target: "zone::payload",
                        l1_block = prepared.header.inner.number,
                        deposits = total_deposits,
                        is_halt = result.is_halt(),
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
        }

        // Execute pool transactions
        // TODO: Use gas accounting from TempoPayloadBuilder (payment vs non-payment limits, etc.)
        let base_fee = builder.evm_mut().block.basefee;
        let mut best_txs = self
            .pool
            .best_transactions_with_attributes(BestTransactionsAttributes::new(base_fee, None));

        while let Some(pool_tx) = best_txs.next() {
            let gas_limit_left = block_gas_limit.saturating_sub(shared_gas_limit);
            if cumulative_gas_used + pool_tx.gas_limit() > gas_limit_left {
                best_txs.mark_invalid(
                    &pool_tx,
                    &InvalidPoolTransactionError::ExceedsGasLimit(
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
        let finalize_tx = build_finalize_withdrawal_batch_tx(U256::MAX, block_number);
        let mut finalize_reverted = false;
        match builder.execute_transaction_with_result_closure(finalize_tx, |result| {
            if !result.is_success() {
                let revert_data = result.output().cloned().unwrap_or_default();
                error!(
                    target: "zone::payload",
                    block_number,
                    is_halt = result.is_halt(),
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
            l1_block = prepared.header.number(),
            l1_hash = ?prepared.header.hash(),
            hash = ?sealed_block.hash(),
            gas_used = sealed_block.gas_used(),
            deposits = total_deposits,
            tx_count = sealed_block.body().transactions.len(),
            ?elapsed,
            "Built zone payload"
        );

        let payload =
            EthBuiltPayload::new(attributes.payload_id(), sealed_block, total_fees, requests);

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
/// This must be the **last** transaction in every zone block. It calls
/// [`ZoneOutbox.finalizeWithdrawalBatch`](crate::abi::ZoneOutbox) which:
/// - Collects up to `count` pending withdrawals
/// - Builds the withdrawal hash chain (oldest outermost)
/// - Increments `withdrawalBatchIndex`
/// - Writes `_lastBatch` to state for proof access
/// - Emits `BatchFinalized`
///
/// Pass `u256::MAX` to batch all pending withdrawals. `block_number` must match the current zone
/// block number.
pub(crate) fn build_finalize_withdrawal_batch_tx(
    count: U256,
    block_number: u64,
) -> Recovered<TempoTxEnvelope> {
    let calldata = abi::ZoneOutbox::finalizeWithdrawalBatchCall {
        count,
        blockNumber: block_number,
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

    use crate::{
        abi::{self, DepositType, ZoneInbox},
        l1::PreparedL1Block,
    };

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
                            memo: B256::ZERO,
                        }),
                    ),
                },
                abi::QueuedDeposit {
                    depositType: DepositType::Encrypted,
                    depositData: alloy_primitives::Bytes::from(
                        alloy_sol_types::SolValue::abi_encode(&abi::EncryptedDeposit {
                            token,
                            sender,
                            amount: 300_000,
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
                },
            ],
            decryptions: vec![abi::DecryptionData {
                sharedSecret: B256::ZERO,
                sharedSecretYParity: 0x02,
                to: sender,
                memo: B256::ZERO,
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
