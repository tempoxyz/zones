//! ABI dispatch and centralized call rules for the [`ZoneOutbox`] precompile.

use alloy_primitives::{Address, Bytes, U256};
use revm::precompile::PrecompileResult;
use tempo_precompiles::{
    Precompile, charge_input_cost, dispatch,
    storage::Handler,
    view,
};
use tempo_zone_contracts::{ILegacyZoneOutbox, IZoneOutbox};
use zone_primitives::constants::{MAX_WITHDRAWAL_GAS_LIMIT, ZONE_CONFIG_ADDRESS};

use crate::{ecies::AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE, tx_context};

use super::{
    MAX_CALLBACK_DATA_SIZE, MAX_GAS_FEE_RATE, REVEAL_TO_KEY_LENGTH, WITHDRAWAL_BASE_GAS, ZoneOutbox,
};

impl Precompile for ZoneOutbox {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        dispatch!(calldata, |call| match call {
            IZoneOutbox::IZoneOutboxCalls {
                config(call) => view(call, |_| Ok(ZONE_CONFIG_ADDRESS)),
                tempoGasRate(call) => view(call, |_| self.tempo_gas_rate.read()),
                maxWithdrawalsPerBlock(call) => view(call, |_| self.max_withdrawals_per_block.read()),
                lastBatch(call) => view(call, |_| self.last_batch()),
                withdrawalBatchIndex(call) => view(call, |_| self.withdrawal_batch_index.read()),
                lastFinalizedTimestamp(call) => view(call, |_| self.last_finalized_timestamp.read()),
                nextWithdrawalIndex(call) => view(call, |_| self.next_withdrawal_index.read()),
                pendingWithdrawalsCount(call) => view(call, |_| self.pending_withdrawals_count()),
                getPendingWithdrawals(call) => view(call, |_| self.get_pending_withdrawals()),
                calculateWithdrawalFee(call) => self.calculate_withdrawal_fee(call.gasLimit),
                MAX_CALLBACK_DATA_SIZE(call) => view(call, |_| Ok(U256::from(MAX_CALLBACK_DATA_SIZE))),
                MAX_WITHDRAWAL_GAS_LIMIT(call) => view(call, |_| Ok(MAX_WITHDRAWAL_GAS_LIMIT)),
                MAX_GAS_FEE_RATE(call) => view(call, |_| Ok(MAX_GAS_FEE_RATE)),
                WITHDRAWAL_BASE_GAS(call) => view(call, |_| Ok(WITHDRAWAL_BASE_GAS)),
                REVEAL_TO_KEY_LENGTH(call) => view(call, |_| Ok(U256::from(REVEAL_TO_KEY_LENGTH))),
                AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH(call) => view(call, |_| Ok(U256::from(AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE))),
                setTempoGasRate(call) => self.set_tempo_gas_rate(call),
                setMaxWithdrawalsPerBlock(call) => self.set_max_withdrawals_per_block(call),
                requestWithdrawal(call) => self.request_withdrawal(
                    msg_sender,
                    tx_context::current_tx_hash().unwrap_or_default(),
                    call,
                ),
                enqueueDepositBounceBack(call) => self.enqueue_deposit_bounce_back(msg_sender, call),
                finalizeWithdrawalBatch(call) => self.finalize_withdrawal_batch(call),
            }
            ILegacyZoneOutbox::ILegacyZoneOutboxCalls {
                requestWithdrawal(call) => self.request_withdrawal(
                    msg_sender,
                    tx_context::current_tx_hash().unwrap_or_default(),
                    IZoneOutbox::requestWithdrawalCall {
                        token: call.token,
                        to: call.to,
                        amount: call.amount,
                        memo: call.memo,
                        gasLimit: call.gasLimit,
                        fallbackRecipient: call.fallbackRecipient,
                        data: call.data,
                        revealTo: Bytes::new(),
                    },
                ),
            }
        })
    }
}
