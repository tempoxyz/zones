//! System transaction builders for zone block construction.
//!
//! System transactions are constructed as legacy transactions with the sentinel system tx
//! signature (`TEMPO_SYSTEM_TX_SIGNATURE`) and sender (`TEMPO_SYSTEM_TX_SENDER`), which
//! bypass normal signature validation and gas accounting in the EVM.
//!
//! Currently provides:
//!
//! - **`finalizeWithdrawalBatch`** — last tx in the block. Builds the withdrawal hash chain
//!   from pending withdrawals and writes batch state for proof generation.

use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::U256;
use alloy_sol_types::SolCall;
use reth_primitives_traits::Recovered;
use tempo_primitives::{
    TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};

use crate::abi::{self, ZONE_OUTBOX_ADDRESS};

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
    let calldata = abi::ZoneOutbox::finalizeWithdrawalBatchCall { count, blockNumber: block_number }.abi_encode();

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


