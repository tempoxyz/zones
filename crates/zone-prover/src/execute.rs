//! Zone block execution for the prover.
//!
//! Executes zone blocks using revm with:
//! - A witness-backed database (not a real state provider)
//! - A proof-based TempoState precompile (not RPC-based)
//! - The Tempo EVM configuration (custom instructions, precompiles)

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{
    Database, Evm, EvmEnv, FromRecoveredTx,
    revm::{context::result::ResultAndState, inspector::NoOpInspector},
};
use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy_sol_types::SolCall;
use reth_primitives_traits::{Recovered, SignerRecoverable};
use revm::DatabaseCommit;
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_evm::evm::TempoEvm;
use tempo_primitives::{
    TempoTxEnvelope, TempoTxType,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};
use tempo_revm::{TempoBlockEnv, TempoTxEnv};
use tracing::debug;

use crate::{
    tempo::TempoStateAccessor,
    types::{
        DecryptionData, DepositType, ProverError, QueuedDeposit, SolChaumPedersenProof,
        SolDecryptionData, SolQueuedDeposit, ZoneBlock, advanceTempoCall,
        finalizeWithdrawalBatchCall,
    },
};

/// Predeploy addresses matching the zone node's ABI constants.
pub const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");
pub const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");
pub const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");
pub const TEMPO_STATE_READER_ADDRESS: Address =
    address!("0x1c00000000000000000000000000000000000004");

/// Result of executing a single zone block.
#[derive(Debug)]
pub struct BlockExecutionResult {
    /// Transactions root (ordered trie of transaction hashes).
    pub transactions_root: B256,
    /// Receipts root (ordered trie of receipt RLP).
    pub receipts_root: B256,
}

/// Execute a zone block within the prover.
///
/// Creates a `TempoEvm`, registers the proof-based precompile, and executes
/// all transactions in order:
/// 1. `advanceTempo` system tx (if block advances Tempo)
/// 2. User transactions
/// 3. `finalizeWithdrawalBatch` system tx (only in final block)
///
/// State changes are committed to the database after each transaction so that
/// subsequent transactions can see prior modifications.
///
/// The caller is responsible for calling `tempo_state.bind_block()` before this
/// function so that the precompile has the correct Tempo block binding.
///
/// Returns the transactions root and receipts root.
pub fn execute_zone_block<DB: Database<Error: core::fmt::Debug> + DatabaseCommit>(
    db: DB,
    block: &ZoneBlock,
    block_index: usize,
    tempo_state: &TempoStateAccessor,
    chain_id: u64,
    is_last_block: bool,
) -> Result<(BlockExecutionResult, DB), ProverError> {
    // Build the EVM environment for this block.
    let evm_env = build_evm_env(block, chain_id);

    // Create the TempoEvm.
    let mut evm: TempoEvm<DB, NoOpInspector> = TempoEvm::new(db, evm_env);

    // Register the proof-based TempoStateReader precompile.
    // Note: bind_block() must have been called by the caller before this point
    // so the precompile's cloned block_bindings includes the current block.
    let precompile = crate::tempo::prover_tempo_state_precompile(tempo_state, block_index);
    {
        let (_, _, precompiles) = evm.components_mut();
        precompiles.apply_precompile(&TEMPO_STATE_READER_ADDRESS, |_| Some(precompile));
    }

    // EIP-2935: store the parent block hash in the history contract.
    // This matches the pre-execution system call applied by the zone node's
    // EthBlockExecutor so the prover produces the same state root.
    if block.number > 0 {
        let result = evm
            .transact_system_call(
                alloy_eips::eip4788::SYSTEM_ADDRESS,
                alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
                block.parent_hash.0.into(),
            )
            .map_err(|e| {
                ProverError::ExecutionError(format!(
                    "EIP-2935 blockhashes system call failed: {e:?}"
                ))
            })?;
        evm.db_mut().commit(result.state);
    }

    // Collect all transactions (as TempoTxEnvelope) and their execution results.
    let mut all_txs: Vec<TempoTxEnvelope> = Vec::new();
    let mut cumulative_gas_used: u64 = 0;
    let mut receipt_envelopes: Vec<ReceiptData> = Vec::new();

    // 1. Execute advanceTempo system tx (if block advances Tempo).
    if let Some(tempo_header_rlp) = &block.tempo_header_rlp {
        let recovered_tx =
            build_advance_tempo_tx(tempo_header_rlp, &block.deposits, &block.decryptions);

        let tempo_block_hash = keccak256(tempo_header_rlp);
        debug!(
            block_number = block.number,
            %tempo_block_hash,
            "advanceTempo system tx"
        );

        let tx_env = <TempoTxEnv as FromRecoveredTx<TempoTxEnvelope>>::from_recovered_tx(
            recovered_tx.inner(),
            recovered_tx.signer(),
        );
        let ResultAndState { result, state } = evm
            .transact_raw(tx_env)
            .map_err(|e| ProverError::ExecutionError(format!("advanceTempo failed: {e:?}")))?;

        // Commit state changes so subsequent transactions see them.
        evm.db_mut().commit(state);

        cumulative_gas_used += result.gas_used();
        let tx_type = recovered_tx.inner().tx_type();

        receipt_envelopes.push(ReceiptData {
            tx_type,
            status: result.is_success(),
            cumulative_gas_used,
            logs: result.logs().to_vec(),
        });
        all_txs.push(recovered_tx.into_inner());
    }

    // 2. Execute user transactions.
    let user_txs = decode_user_transactions(&block.transactions)?;
    for (i, recovered_tx) in user_txs.into_iter().enumerate() {
        let tx_env = <TempoTxEnv as FromRecoveredTx<TempoTxEnvelope>>::from_recovered_tx(
            recovered_tx.inner(),
            recovered_tx.signer(),
        );
        let ResultAndState { result, state } = evm
            .transact_raw(tx_env)
            .map_err(|e| ProverError::ExecutionError(format!("user tx {i} failed: {e:?}")))?;

        evm.db_mut().commit(state);

        cumulative_gas_used += result.gas_used();
        let tx_type = recovered_tx.inner().tx_type();

        receipt_envelopes.push(ReceiptData {
            tx_type,
            status: result.is_success(),
            cumulative_gas_used,
            logs: result.logs().to_vec(),
        });
        all_txs.push(recovered_tx.into_inner());
    }

    // 3. Execute finalizeWithdrawalBatch system tx (only in final block).
    if is_last_block {
        if let Some(count) = block.finalize_withdrawal_batch_count {
            let recovered_tx = build_finalize_withdrawal_batch_tx(count, block.number);

            let tx_env = <TempoTxEnv as FromRecoveredTx<TempoTxEnvelope>>::from_recovered_tx(
                recovered_tx.inner(),
                recovered_tx.signer(),
            );
            let ResultAndState { result, state } = evm.transact_raw(tx_env).map_err(|e| {
                ProverError::ExecutionError(format!("finalizeWithdrawalBatch failed: {e:?}"))
            })?;

            evm.db_mut().commit(state);

            cumulative_gas_used += result.gas_used();
            let tx_type = recovered_tx.inner().tx_type();

            receipt_envelopes.push(ReceiptData {
                tx_type,
                status: result.is_success(),
                cumulative_gas_used,
                logs: result.logs().to_vec(),
            });
            all_txs.push(recovered_tx.into_inner());
        }
    }

    // Compute transactions root and receipts root.
    let transactions_root = compute_transactions_root(&all_txs);
    let receipts_root = compute_receipts_root(&receipt_envelopes);

    // Extract the database back from the EVM.
    let (db, _env) = evm.finish();

    Ok((
        BlockExecutionResult {
            transactions_root,
            receipts_root,
        },
        db,
    ))
}

/// Build the EVM environment for a zone block.
fn build_evm_env(block: &ZoneBlock, chain_id: u64) -> EvmEnv<TempoHardfork, TempoBlockEnv> {
    use revm::context::{BlockEnv, CfgEnv};

    let block_env = TempoBlockEnv {
        inner: BlockEnv {
            number: U256::from(block.number),
            beneficiary: block.beneficiary,
            timestamp: U256::from(block.timestamp),
            gas_limit: block.gas_limit,
            basefee: block.base_fee_per_gas,
            ..Default::default()
        },
        timestamp_millis_part: 0,
    };

    let mut cfg_env = CfgEnv::new_with_spec(TempoHardfork::T0);
    cfg_env.chain_id = chain_id;

    EvmEnv { cfg_env, block_env }
}

/// Internal receipt data used to compute the receipts root.
struct ReceiptData {
    tx_type: TempoTxType,
    status: bool,
    cumulative_gas_used: u64,
    logs: Vec<alloy_primitives::Log>,
}

// ---------------------------------------------------------------------------
//  Transaction and receipt root computation
// ---------------------------------------------------------------------------

fn compute_transactions_root(txs: &[TempoTxEnvelope]) -> B256 {
    alloy_consensus::proofs::calculate_transaction_root(txs)
}

fn compute_receipts_root(receipts: &[ReceiptData]) -> B256 {
    if receipts.is_empty() {
        return alloy_trie::EMPTY_ROOT_HASH;
    }

    use alloy_trie::HashBuilder;
    use nybbles::Nibbles;

    let mut leaves: Vec<(Nibbles, Vec<u8>)> = receipts
        .iter()
        .enumerate()
        .map(|(i, receipt)| {
            let key = {
                let mut buf = Vec::new();
                alloy_rlp::Encodable::encode(&i, &mut buf);
                Nibbles::unpack(&buf)
            };
            let value = encode_receipt_2718(receipt);
            (key, value)
        })
        .collect();

    leaves.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut hb = HashBuilder::default();
    for (key, value) in &leaves {
        hb.add_leaf(key.clone(), value);
    }
    hb.root()
}

/// Encode a receipt in EIP-2718 form for the receipts trie:
/// - Legacy: `rlp(receipt_payload)`
/// - Typed:  `txType || rlp(receipt_payload)`
fn encode_receipt_2718(receipt: &ReceiptData) -> Vec<u8> {
    let payload = encode_receipt_payload_rlp(receipt);
    if receipt.tx_type == TempoTxType::Legacy {
        payload
    } else {
        let mut typed = Vec::with_capacity(1 + payload.len());
        typed.push(receipt.tx_type as u8);
        typed.extend_from_slice(&payload);
        typed
    }
}

/// Encode the legacy receipt payload as:
/// `[status, cumulativeGasUsed, logsBloom, logs]`.
fn encode_receipt_payload_rlp(receipt: &ReceiptData) -> Vec<u8> {
    use alloy_primitives::Bloom;

    // Build the logs bloom from the receipt's logs.
    let bloom = {
        let mut bloom = Bloom::default();
        for log in &receipt.logs {
            bloom.accrue_log(log);
        }
        bloom
    };

    let status: bool = receipt.status;
    let cumulative_gas_used = receipt.cumulative_gas_used;
    let logs = &receipt.logs;

    // Compute the logs list RLP length.
    let logs_list_payload: usize = logs.iter().map(|l| alloy_rlp::Encodable::length(l)).sum();
    let logs_len = alloy_rlp::Header {
        list: true,
        payload_length: logs_list_payload,
    }
    .length()
        + logs_list_payload;

    // Compute the outer list payload length.
    let status_len = alloy_rlp::Encodable::length(&status);
    let gas_len = alloy_rlp::Encodable::length(&cumulative_gas_used);
    let bloom_len = alloy_rlp::Encodable::length(&bloom);
    let payload_len = status_len + gas_len + bloom_len + logs_len;

    let mut buf = Vec::with_capacity(payload_len + 5);
    alloy_rlp::Header {
        list: true,
        payload_length: payload_len,
    }
    .encode(&mut buf);

    alloy_rlp::Encodable::encode(&status, &mut buf);
    alloy_rlp::Encodable::encode(&cumulative_gas_used, &mut buf);
    alloy_rlp::Encodable::encode(&bloom, &mut buf);
    alloy_rlp::encode_list(logs, &mut buf);

    buf
}

// ---------------------------------------------------------------------------
//  Transaction builders
// ---------------------------------------------------------------------------

/// Build the `advanceTempo(header, deposits, decryptions)` system transaction.
///
/// This mirrors `system_tx::build_advance_tempo_tx` in the zone node, but
/// takes the raw data from the prover's [`ZoneBlock`] instead of node-side types.
pub fn build_advance_tempo_tx(
    tempo_header_rlp: &[u8],
    deposits: &[QueuedDeposit],
    decryptions: &[DecryptionData],
) -> Recovered<TempoTxEnvelope> {
    let sol_deposits: Vec<SolQueuedDeposit> = deposits
        .iter()
        .map(|d| SolQueuedDeposit {
            depositType: match d.deposit_type {
                DepositType::Regular => 0,
                DepositType::Encrypted => 1,
            },
            depositData: d.deposit_data.clone(),
        })
        .collect();

    let sol_decryptions: Vec<SolDecryptionData> = decryptions
        .iter()
        .map(|d| SolDecryptionData {
            sharedSecret: d.shared_secret,
            sharedSecretYParity: d.shared_secret_y_parity,
            to: d.to,
            memo: d.memo,
            cpProof: SolChaumPedersenProof {
                s: d.cp_proof.s,
                c: d.cp_proof.c,
            },
        })
        .collect();

    let calldata = advanceTempoCall {
        header: Bytes::copy_from_slice(tempo_header_rlp),
        deposits: sol_deposits,
        decryptions: sol_decryptions,
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

/// Build the `finalizeWithdrawalBatch(count, blockNumber)` system transaction.
///
/// Mirrors `system_tx::build_finalize_withdrawal_batch_tx` in the zone node.
pub fn build_finalize_withdrawal_batch_tx(
    count: U256,
    block_number: u64,
) -> Recovered<TempoTxEnvelope> {
    let calldata = finalizeWithdrawalBatchCall {
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

/// Decode user transactions from RLP-encoded bytes.
///
/// Each entry in `raw_txs` is an RLP-encoded `TempoTxEnvelope`.
pub fn decode_user_transactions(
    raw_txs: &[Vec<u8>],
) -> Result<Vec<Recovered<TempoTxEnvelope>>, ProverError> {
    let mut txs = Vec::with_capacity(raw_txs.len());
    for (i, raw) in raw_txs.iter().enumerate() {
        let envelope: TempoTxEnvelope = alloy_rlp::Decodable::decode(&mut raw.as_slice())
            .map_err(|e| ProverError::RlpDecode(format!("transaction {i}: {e}")))?;

        // Recover the sender from the signature.
        let recovered = envelope.try_into_recovered().map_err(|e| {
            ProverError::ExecutionError(format!("transaction {i} signature recovery: {e}"))
        })?;

        txs.push(recovered);
    }
    Ok(txs)
}

/// Storage layout helpers for reading zone system contract state.
///
/// These match the Solidity storage layout of the zone predeploy contracts.
pub mod storage {
    use alloy_primitives::U256;

    /// TempoState slot 0: `tempoBlockHash` (bytes32).
    pub const TEMPO_STATE_BLOCK_HASH_SLOT: U256 = U256::ZERO;

    /// TempoState slot 4: `tempoStateRoot` (bytes32).
    pub const TEMPO_STATE_STATE_ROOT_SLOT: U256 = U256::from_limbs([4, 0, 0, 0]);

    /// TempoState slot 7: packed `(tempoBlockNumber, tempoGasLimit, tempoGasUsed, tempoTimestamp)`.
    /// `tempoBlockNumber` is the lowest uint64 (offset 0).
    pub const TEMPO_STATE_PACKED_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

    /// ZoneInbox: `processedDepositQueueHash` is at slot 0.
    pub const ZONE_INBOX_PROCESSED_HASH_SLOT: U256 = U256::ZERO;

    /// ZoneOutbox: `_lastBatch` is at a fixed storage slot.
    /// Slot layout depends on the contract's storage, but `_lastBatch` is a struct
    /// that occupies two slots.
    ///
    /// The `lastBatch()` function returns the struct. For direct storage reads:
    /// - `_lastBatch.withdrawalQueueHash` is at the base slot
    /// - `_lastBatch.withdrawalBatchIndex` is at base + 1
    ///
    /// The exact slot depends on the number of preceding storage variables in ZoneOutbox.
    /// We compute this from the Solidity layout.
    pub const ZONE_OUTBOX_LAST_BATCH_BASE_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);

    /// Extract `tempoBlockNumber` from the packed TempoState slot 7.
    pub fn extract_tempo_block_number(packed: U256) -> u64 {
        (packed & U256::from(u64::MAX)).to::<u64>()
    }
}
