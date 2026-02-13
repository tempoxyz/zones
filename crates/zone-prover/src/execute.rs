//! Zone block execution for the prover.
//!
//! Executes zone blocks using revm with:
//! - A witness-backed database (not a real state provider)
//! - A proof-based TempoState precompile (not RPC-based)
//! - The Tempo EVM configuration (custom instructions, precompiles)

use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::{Address, Bytes, U256, address};
use alloy_sol_types::SolCall;
use reth_primitives_traits::{Recovered, SignerRecoverable};
use tempo_primitives::{
    TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};

use crate::types::{
    DecryptionData, DepositType, ProverError, QueuedDeposit,
    advanceTempoCall, finalizeWithdrawalBatchCall,
    SolQueuedDeposit, SolDecryptionData, SolChaumPedersenProof,
};

/// Predeploy addresses matching the zone node's ABI constants.
pub const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");
pub const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");
pub const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");
pub const TEMPO_STATE_READER_ADDRESS: Address =
    address!("0x1c00000000000000000000000000000000000004");

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
        let envelope: TempoTxEnvelope =
            alloy_rlp::Decodable::decode(&mut raw.as_slice()).map_err(|e| {
                ProverError::RlpDecode(format!("transaction {i}: {e}"))
            })?;

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

    /// TempoState slot 7: packed `(tempoBlockNumber, tempoGasLimit, tempoGasUsed, tempoTimestamp)`.
    /// `tempoBlockNumber` is the lowest uint64 (offset 0).
    pub const TEMPO_STATE_PACKED_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

    /// ZoneInbox: `processedDepositQueueHash` is at slot 1
    /// (after `config`, `tempoPortal`, `_tempoState`, `zoneToken` which are immutable,
    /// and the first mutable storage variable).
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
