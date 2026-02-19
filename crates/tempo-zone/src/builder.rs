//! Zone payload builder.
//!
//! Builds zone blocks by executing `advanceTempo` system transactions (one per L1 block)
//! followed by pool transactions and a withdrawal batch finalization.

use crate::{
    abi::{self, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS},
    evm::ZoneEvmConfig,
    l1::L1Deposit,
    precompiles::ecies,
};
use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::{Address, Bytes, U256};
use alloy_rlp::Encodable;
use alloy_sol_types::{SolCall, SolValue};
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
use tempo_payload_types::TempoPayloadBuilderAttributes;
use tempo_primitives::{
    TempoHeader, TempoPrimitives, TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};
use tempo_transaction_pool::TempoTransactionPool;
use tracing::{debug, error, info, warn};

use super::node::ZoneNode;

/// Factory for constructing the zone payload builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZonePayloadFactory {
    deposit_queue: crate::DepositQueue,
    sequencer: Option<Address>,
    /// Sequencer's secp256k1 secret key for ECIES decryption of encrypted deposits.
    sequencer_key: Option<k256::SecretKey>,
    /// ZonePortal address on L1 — used as context in HKDF key derivation.
    portal_address: Address,
}

impl ZonePayloadFactory {
    pub fn new(
        deposit_queue: crate::DepositQueue,
        sequencer: Option<Address>,
        sequencer_key: Option<k256::SecretKey>,
        portal_address: Address,
    ) -> Self {
        Self {
            deposit_queue,
            sequencer,
            sequencer_key,
            portal_address,
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
            _sequencer: self.sequencer,
            sequencer_key: self.sequencer_key,
            portal_address: self.portal_address,
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
    _sequencer: Option<Address>,
    sequencer_key: Option<k256::SecretKey>,
    portal_address: Address,
}

impl<Provider> ZonePayloadBuilder<Provider> {
    pub fn new(
        pool: TempoTransactionPool<Provider>,
        provider: Provider,
        evm_config: ZoneEvmConfig,
        deposit_queue: crate::DepositQueue,
        sequencer: Option<Address>,
        sequencer_key: Option<k256::SecretKey>,
        portal_address: Address,
    ) -> Self {
        Self {
            pool,
            provider,
            evm_config,
            deposit_queue,
            _sequencer: sequencer,
            sequencer_key,
            portal_address,
        }
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
                debug!(target: "zone::payload", "No L1 block available, skipping build");
                return Err(PayloadBuilderError::Internal(reth_errors::RethError::msg(
                    "no L1 block available in deposit queue",
                )));
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
            match deposit {
                L1Deposit::Regular(d) => {
                    debug!(
                        target: "zone::payload",
                        sender = %d.sender,
                        to = %d.to,
                        amount = %d.amount,
                        l1_block = l1_block.header.inner.number,
                        "Regular deposit -> advanceTempo"
                    );
                }
                L1Deposit::Encrypted(d) => {
                    debug!(
                        target: "zone::payload",
                        sender = %d.sender,
                        amount = %d.amount,
                        key_index = %d.key_index,
                        l1_block = l1_block.header.inner.number,
                        "Encrypted deposit -> advanceTempo"
                    );
                }
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

        // Execute advanceTempo system transaction — exactly one per zone block.
        {
            // Log header details for debugging chain continuity
            let header_rlp = alloy_rlp::encode(&l1_block.header);
            info!(
                target: "zone::payload",
                l1_block_number = l1_block.header.inner.number,
                l1_parent_hash = %l1_block.header.inner.parent_hash,
                l1_block_hash = %alloy_primitives::keccak256(&header_rlp),
                header_rlp_len = header_rlp.len(),
                "advanceTempo header details"
            );

            let advance_tx = build_advance_tempo_tx(
                &l1_block.header,
                &l1_block.deposits,
                self.sequencer_key.as_ref(),
                self.portal_address,
            );
            let mut reverted = false;
            match builder.execute_transaction_with_result_closure(advance_tx, |result| {
                if !result.is_success() {
                    let revert_data = result.output().cloned().unwrap_or_default();
                    error!(
                        target: "zone::payload",
                        l1_block = l1_block.header.inner.number,
                        deposits = l1_block.deposits.len(),
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
                            l1_block.header.inner.number
                        ),
                    )));
                }
                Ok(_) => {}
                Err(err) => {
                    error!(
                        ?err,
                        l1_block = l1_block.header.inner.number,
                        deposits = l1_block.deposits.len(),
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

/// Build the `advanceTempo(header, deposits, decryptions)` system transaction.
///
/// This must be called **once per L1 block** at the start of a zone block (before user txs).
/// It calls [`ZoneInbox.advanceTempo`](crate::abi::ZoneInbox) which atomically:
/// - Advances the zone's view of Tempo by processing the L1 block header
/// - Processes deposits from the queue (minting zone tokens to recipients)
/// - Validates the deposit hash chain against Tempo state
///
/// Regular deposits are wrapped as `QueuedDeposit` with `DepositType::Regular`.
/// Encrypted deposits are wrapped with `DepositType::Encrypted` and paired with
/// `DecryptionData` entries that the sequencer provides after decrypting.
pub fn build_advance_tempo_tx(
    header: &TempoHeader,
    deposits: &[L1Deposit],
    sequencer_key: Option<&k256::SecretKey>,
    portal_address: Address,
) -> Recovered<TempoTxEnvelope> {
    // RLP-encode the Tempo header
    let mut header_rlp = Vec::new();
    header.encode(&mut header_rlp);

    let mut queued_deposits: Vec<abi::QueuedDeposit> = Vec::new();
    let mut decryptions: Vec<abi::DecryptionData> = Vec::new();

    for d in deposits {
        match d {
            L1Deposit::Regular(d) => {
                let deposit = abi::Deposit {
                    token: d.token,
                    sender: d.sender,
                    to: d.to,
                    amount: d.amount,
                    memo: d.memo,
                };
                queued_deposits.push(abi::QueuedDeposit {
                    depositType: abi::DepositType::Regular,
                    depositData: Bytes::from(deposit.abi_encode()),
                });
            }
            L1Deposit::Encrypted(d) => {
                let encrypted = abi::EncryptedDeposit {
                    token: d.token,
                    sender: d.sender,
                    amount: d.amount,
                    keyIndex: d.key_index,
                    encrypted: abi::EncryptedDepositPayload {
                        ephemeralPubkeyX: d.ephemeral_pubkey_x,
                        ephemeralPubkeyYParity: d.ephemeral_pubkey_y_parity,
                        ciphertext: d.ciphertext.clone().into(),
                        nonce: d.nonce.into(),
                        tag: d.tag.into(),
                    },
                };
                queued_deposits.push(abi::QueuedDeposit {
                    depositType: abi::DepositType::Encrypted,
                    depositData: Bytes::from(encrypted.abi_encode()),
                });

                // Decrypt the encrypted deposit using the sequencer's private key.
                // If no key is available or decryption fails, use placeholder values —
                // the ZoneInbox contract will bounce funds back to sender on failure.
                let dec = sequencer_key.and_then(|key| {
                    ecies::decrypt_deposit(
                        key,
                        &d.ephemeral_pubkey_x,
                        d.ephemeral_pubkey_y_parity,
                        &d.ciphertext,
                        &d.nonce,
                        &d.tag,
                        portal_address,
                        d.key_index,
                    )
                });

                if let Some(dec) = dec {
                    decryptions.push(abi::DecryptionData {
                        sharedSecret: dec.shared_secret,
                        sharedSecretYParity: dec.shared_secret_y_parity,
                        to: dec.to,
                        memo: dec.memo,
                        cpProof: abi::ChaumPedersenProof {
                            s: dec.cp_proof_s,
                            c: dec.cp_proof_c,
                        },
                    });
                } else {
                    warn!(
                        target: "zone::payload",
                        sender = %d.sender,
                        amount = %d.amount,
                        "Encrypted deposit decryption failed, using placeholder DecryptionData"
                    );
                    decryptions.push(abi::DecryptionData {
                        sharedSecret: alloy_primitives::B256::ZERO,
                        sharedSecretYParity: 0x02,
                        to: alloy_primitives::Address::ZERO,
                        memo: alloy_primitives::B256::ZERO,
                        cpProof: abi::ChaumPedersenProof {
                            s: alloy_primitives::B256::ZERO,
                            c: alloy_primitives::B256::ZERO,
                        },
                    });
                }
            }
        }
    }

    let calldata = abi::ZoneInbox::advanceTempoCall {
        header: Bytes::from(header_rlp),
        deposits: queued_deposits,
        decryptions,
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
    use tempo_primitives::TempoHeader;

    use crate::{
        abi::{DepositType, ZoneInbox},
        l1::{Deposit, EncryptedDeposit, L1Deposit},
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

        let regular = Deposit {
            l1_block_number: 1,
            token,
            sender,
            to: recipient,
            amount: 500_000,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        let encrypted = EncryptedDeposit {
            l1_block_number: 1,
            token,
            sender,
            amount: 300_000,
            fee: 0,
            key_index: U256::ZERO,
            ephemeral_pubkey_x: B256::with_last_byte(0xDD),
            ephemeral_pubkey_y_parity: 0x02,
            ciphertext: vec![0xAA; 64],
            nonce: [0x05; 12],
            tag: [0x06; 16],
            queue_hash: B256::ZERO,
        };

        let deposits = vec![L1Deposit::Regular(regular), L1Deposit::Encrypted(encrypted)];

        // Build the system transaction (no sequencer key → placeholder DecryptionData)
        let portal_address = address!("0x0000000000000000000000000000000000000001");
        let recovered_tx = super::build_advance_tempo_tx(&header, &deposits, None, portal_address);

        // Decode the calldata to verify structure.
        // Recovered<TempoTxEnvelope> → deref → TempoTxEnvelope::Legacy(Signed<TxLegacy>)
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
