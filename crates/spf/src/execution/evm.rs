//! Tempo EVM setup and Zone-block execution.

use alloy_consensus::{
    Signed, TxLegacy,
    transaction::{Recovered, SignerRecoverable as _},
};
use alloy_eips::eip2718::Decodable2718 as _;
use alloy_evm::{EvmEnv, EvmFactory as _, block::BlockExecutor as _, eth::EthBlockExecutionCtx};
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall as _;
use revm::{
    context::{BlockEnv, CfgEnv},
    database::{State, states::bundle_state::BundleRetention},
    database_interface::bal::EvmDatabaseError,
};
use tempo_chainspec::{TempoHardforks, hardfork::TempoHardfork};
use tempo_evm::{TempoBlockEnv, TempoBlockExecutionCtx};
use tempo_primitives::{
    TempoReceipt, TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};
use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZoneInbox, ZoneOutbox};
use zone_evm::{ZoneBlockExecutor, ZoneEvmFactory};
use zone_primitives::constants::zone_chain_id;

use crate::{
    Error, SpfConfig, ZoneBlock,
    execution::database::{TempoWitnessDatabase, WitnessDatabase},
};

type ZoneState = State<WitnessDatabase>;

/// Execution artifacts committed by one Zone block header.
#[derive(Debug)]
pub(crate) struct ExecutedZoneBlock {
    pub(crate) transactions: Vec<TempoTxEnvelope>,
    pub(crate) receipts: Vec<TempoReceipt>,
}

/// REVM configuration and block environment prepared for one Zone block.
#[derive(Debug, Clone)]
pub(crate) struct ZoneEvmEnv {
    pub(crate) cfg: CfgEnv<TempoHardfork>,
    pub(crate) block: TempoBlockEnv,
}

impl ZoneEvmEnv {
    /// Construct the Tempo execution environment for one Zone block.
    ///
    /// The Zone EVM uses the parent Tempo fork schedule at the Zone block
    /// timestamp and has no protocol base fee. This matches
    /// [`ZoneEvmConfig::next_evm_env`] in the block builder.
    ///
    /// The simplified Zone header does not commit a gas limit, so SPF receives
    /// the fixed network value through [`SpfConfig`].
    pub(crate) fn new(config: &SpfConfig, zone_id: u32, block: &ZoneBlock) -> Self {
        let mut cfg =
            CfgEnv::new_with_spec(config.zone_chain_spec.tempo_hardfork_at(block.timestamp));
        cfg.chain_id = zone_chain_id(zone_id);

        Self {
            cfg,
            block: TempoBlockEnv {
                inner: BlockEnv {
                    number: U256::from(block.number),
                    beneficiary: block.beneficiary,
                    timestamp: U256::from(block.timestamp),
                    gas_limit: config.block_gas_limit,
                    basefee: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }

    /// Build the production block-execution context for this Zone block.
    fn execution_context(&self, block: &ZoneBlock) -> TempoBlockExecutionCtx<'static> {
        TempoBlockExecutionCtx {
            inner: EthBlockExecutionCtx {
                parent_hash: block.parent_hash,
                parent_beacon_block_root: None,
                ommers: &[],
                withdrawals: None,
                extra_data: Bytes::new(),
                tx_count_hint: None,
                slot_number: None,
            },
            general_gas_limit: 0,
            shared_gas_limit: self.block.inner.gas_limit,
            validator_set: None,
            consensus_context: None,
            subblock_fee_recipients: Default::default(),
        }
    }
}

/// Execute a complete Zone block in system-then-user order.
///
/// When a Tempo header is present, `ZoneInbox.advanceTempo` executes first.
/// That call invokes `TempoState.finalizeTempo`, then processes deposits and
/// enabled tokens. User transactions run only after that system transition.
pub(crate) fn execute_zone_block(
    env: &ZoneEvmEnv,
    config: &SpfConfig,
    zone_state: &mut ZoneState,
    tempo_database: &TempoWitnessDatabase,
    zone_block_index: usize,
    sequencer: Address,
    block: &ZoneBlock,
) -> Result<ExecutedZoneBlock, Error> {
    let user_transactions = decode_user_transactions(zone_block_index, &block.transactions)?;
    let mut transactions = Vec::with_capacity(
        user_transactions.len()
            + usize::from(block.tempo_header_rlp.is_some())
            + usize::from(block.finalize_withdrawal_batch_count.is_some()),
    );
    let parent_number = block
        .number
        .checked_sub(1)
        .ok_or(Error::BlockNumberOverflow)?;
    if let Some(existing) = zone_state.block_hashes.get(parent_number)
        && existing != block.parent_hash
    {
        return Err(crate::WitnessDatabaseError::ConflictingBlockHash {
            number: parent_number,
            expected: existing,
            actual: block.parent_hash,
        }
        .into());
    }
    zone_state
        .block_hashes
        .insert(parent_number, block.parent_hash);

    let tempo_reader = tempo_database.for_sequencer(sequencer);
    let factory = ZoneEvmFactory::new(tempo_reader);
    let evm = factory.create_evm(
        &mut *zone_state,
        EvmEnv::new(env.cfg.clone(), env.block.clone()),
    );
    let mut executor = ZoneBlockExecutor::new(
        evm,
        env.execution_context(block),
        config.zone_chain_spec.as_ref(),
    );

    executor.apply_pre_execution_changes().map_err(|error| {
        map_block_execution_error(
            error,
            Error::BlockPreExecution {
                block_index: zone_block_index,
            },
        )
    })?;

    if let Some(header) = &block.tempo_header_rlp {
        transactions.push(execute_advance_tempo(
            &mut executor,
            header,
            block,
            zone_block_index,
        )?);
    }
    transactions.extend(execute_user_transactions(
        &mut executor,
        zone_block_index,
        user_transactions,
    )?);
    if let Some(count) = block.finalize_withdrawal_batch_count {
        transactions.push(execute_finalize_withdrawal_batch(
            &mut executor,
            count,
            block.number,
            block.finalize_withdrawal_batch_encrypted_senders.clone(),
            zone_block_index,
        )?);
    }

    let (_, output) = executor.finish().map_err(|error| {
        map_block_execution_error(
            error,
            Error::BlockPostExecution {
                block_index: zone_block_index,
            },
        )
    })?;
    zone_state.merge_transitions(BundleRetention::Reverts);

    Ok(ExecutedZoneBlock {
        transactions,
        receipts: output.receipts,
    })
}

fn execute_advance_tempo<'a, 'db, I>(
    executor: &mut ZoneBlockExecutor<'a, &'db mut ZoneState, I>,
    header: &Bytes,
    block: &ZoneBlock,
    block_index: usize,
) -> Result<TempoTxEnvelope, Error>
where
    I: alloy_evm::revm::Inspector<tempo_revm::evm::TempoContext<&'db mut ZoneState>>,
{
    let calldata = ZoneInbox::advanceTempoCall {
        header: header.clone(),
        deposits: block.deposits.clone(),
        decryptions: block.decryptions.clone(),
        enabledTokens: block.enabled_tokens.clone(),
    }
    .abi_encode();
    let transaction = TxLegacy {
        chain_id: None,
        nonce: 0,
        gas_price: 0,
        gas_limit: 0,
        to: ZONE_INBOX_ADDRESS.into(),
        value: U256::ZERO,
        input: calldata.into(),
    };
    let transaction =
        TempoTxEnvelope::Legacy(Signed::new_unhashed(transaction, TEMPO_SYSTEM_TX_SIGNATURE));
    let recovered = Recovered::new_unchecked(transaction.clone(), TEMPO_SYSTEM_TX_SENDER);

    execute_recovered_transaction(
        executor,
        recovered,
        Error::AdvanceTempoExecution { block_index },
        true,
    )?;
    Ok(transaction)
}

fn execute_finalize_withdrawal_batch<'a, 'db, I>(
    executor: &mut ZoneBlockExecutor<'a, &'db mut ZoneState, I>,
    count: U256,
    block_number: u64,
    encrypted_senders: Vec<Bytes>,
    block_index: usize,
) -> Result<TempoTxEnvelope, Error>
where
    I: alloy_evm::revm::Inspector<tempo_revm::evm::TempoContext<&'db mut ZoneState>>,
{
    let calldata = ZoneOutbox::finalizeWithdrawalBatchCall {
        count,
        blockNumber: block_number,
        encryptedSenders: encrypted_senders,
    }
    .abi_encode();
    let transaction = TxLegacy {
        chain_id: None,
        nonce: 0,
        gas_price: 0,
        gas_limit: 0,
        to: ZONE_OUTBOX_ADDRESS.into(),
        value: U256::ZERO,
        input: calldata.into(),
    };
    let transaction =
        TempoTxEnvelope::Legacy(Signed::new_unhashed(transaction, TEMPO_SYSTEM_TX_SIGNATURE));
    let recovered = Recovered::new_unchecked(transaction.clone(), TEMPO_SYSTEM_TX_SENDER);

    execute_recovered_transaction(
        executor,
        recovered,
        Error::FinalizeWithdrawalBatchExecution { block_index },
        true,
    )?;
    Ok(transaction)
}

fn decode_user_transactions(
    block_index: usize,
    transactions: &[Bytes],
) -> Result<Vec<Recovered<TempoTxEnvelope>>, Error> {
    let mut decoded = Vec::with_capacity(transactions.len());
    for (transaction_index, encoded_transaction) in transactions.iter().enumerate() {
        let transaction =
            TempoTxEnvelope::decode_2718_exact(encoded_transaction).map_err(|_| {
                Error::TransactionDecoding {
                    block_index,
                    transaction_index,
                }
            })?;
        if transaction.is_system_tx() {
            return Err(Error::SystemTransactionInUserList {
                block_index,
                transaction_index,
            });
        }
        let signer = transaction
            .recover_signer()
            .map_err(|_| Error::TransactionSignature {
                block_index,
                transaction_index,
            })?;
        decoded.push(Recovered::new_unchecked(transaction, signer));
    }
    Ok(decoded)
}

fn execute_user_transactions<'a, 'db, I>(
    executor: &mut ZoneBlockExecutor<'a, &'db mut ZoneState, I>,
    block_index: usize,
    transactions: Vec<Recovered<TempoTxEnvelope>>,
) -> Result<Vec<TempoTxEnvelope>, Error>
where
    I: alloy_evm::revm::Inspector<tempo_revm::evm::TempoContext<&'db mut ZoneState>>,
{
    let mut executed = Vec::with_capacity(transactions.len());
    for (transaction_index, transaction) in transactions.into_iter().enumerate() {
        let envelope = transaction.clone_inner();
        execute_recovered_transaction(
            executor,
            transaction,
            Error::TransactionExecution {
                block_index,
                transaction_index,
            },
            false,
        )?;
        executed.push(envelope);
    }

    Ok(executed)
}

fn execute_recovered_transaction<'a, 'db, I>(
    executor: &mut ZoneBlockExecutor<'a, &'db mut ZoneState, I>,
    transaction: Recovered<TempoTxEnvelope>,
    execution_error: Error,
    require_success: bool,
) -> Result<(), Error>
where
    I: alloy_evm::revm::Inspector<tempo_revm::evm::TempoContext<&'db mut ZoneState>>,
{
    let result = executor
        .execute_transaction_without_commit(transaction)
        .map_err(|error| map_block_execution_error(error, execution_error))?;
    if require_success && !result.result.result.is_success() {
        return Err(execution_error);
    }
    executor.commit_transaction(result);
    Ok(())
}

fn map_block_execution_error(
    error: alloy_evm::block::BlockExecutionError,
    execution_error: Error,
) -> Error {
    type WitnessEvmError = revm::context::result::EVMError<
        EvmDatabaseError<crate::WitnessDatabaseError>,
        tempo_evm::TempoInvalidTransaction,
    >;

    if let Some(revm::context::result::EVMError::Database(EvmDatabaseError::Database(error))) =
        error
            .as_internal()
            .and_then(|error| error.downcast_evm::<WitnessEvmError>())
    {
        return (*error).into();
    }

    execution_error
}
