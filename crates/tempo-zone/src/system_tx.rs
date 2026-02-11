//! System transaction builders for zone block construction.
//!
//! System transactions are constructed as legacy transactions with the sentinel system tx
//! signature (`TEMPO_SYSTEM_TX_SIGNATURE`) and sender (`TEMPO_SYSTEM_TX_SENDER`), which
//! bypass normal signature validation and gas accounting in the EVM.
//!
//! Currently provides:
//!
//! - **`advanceTempo`** — first system tx(s) in the block. One per L1 block processed.
//!   Advances the zone's view of Tempo state and processes deposits atomically.
//!
//! - **`finalizeWithdrawalBatch`** — last tx in the block. Builds the withdrawal hash chain
//!   from pending withdrawals and writes batch state for proof generation.

use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::{Address, Bytes, U256};
use alloy_rlp::Encodable;
use alloy_sol_types::{SolCall, SolValue};
use reth_primitives_traits::Recovered;
use tempo_primitives::{
    TempoHeader, TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};

use crate::{
    abi::{self, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS},
    l1::Deposit,
};

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
pub fn build_finalize_withdrawal_batch_tx(
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
/// Each deposit is wrapped as a `QueuedDeposit` with `DepositType::Regular`.
/// Encrypted deposits are not yet supported, so `decryptions` is always empty.
pub fn build_advance_tempo_tx(
    header: &TempoHeader,
    deposits: &[Deposit],
    sequencer: Address,
) -> Recovered<TempoTxEnvelope> {
    // RLP-encode the Tempo header
    let mut header_rlp = Vec::new();
    header.encode(&mut header_rlp);

    // Wrap each deposit as a QueuedDeposit with DepositType::Regular
    let queued_deposits: Vec<abi::QueuedDeposit> = deposits
        .iter()
        .map(|d| {
            let deposit = abi::Deposit {
                sender: d.sender,
                to: d.to,
                amount: d.amount,
                memo: d.memo,
            };
            abi::QueuedDeposit {
                depositType: abi::DepositType::Regular,
                depositData: Bytes::from(deposit.abi_encode()),
            }
        })
        .collect();

    // No encrypted deposits yet
    let decryptions: Vec<abi::DecryptionData> = Vec::new();

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
        sequencer,
    )
}
