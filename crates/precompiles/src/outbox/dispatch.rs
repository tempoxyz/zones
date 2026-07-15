//! ABI dispatch for the [`ZoneOutbox`] precompile.

use alloy_primitives::{Address, U256};
use revm::precompile::PrecompileResult;
use tempo_precompiles::{Precompile, charge_input_cost, dispatch, storage::Handler};
use tempo_zone_contracts::{ILegacyZoneOutbox, IZoneOutbox};
use zone_primitives::constants::{MAX_WITHDRAWAL_GAS_LIMIT, ZONE_CONFIG_ADDRESS};

use crate::{
    dispatch::{metadata, mutate, mutate_void, view},
    ecies::AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE,
    tx_context,
};

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
                config(_) => metadata::<IZoneOutbox::configCall>(|| Ok(ZONE_CONFIG_ADDRESS)),
                tempoGasRate(_) => metadata::<IZoneOutbox::tempoGasRateCall>(|| self.tempo_gas_rate.read()),
                maxWithdrawalsPerBlock(_) => metadata::<IZoneOutbox::maxWithdrawalsPerBlockCall>(|| self.max_withdrawals_per_block.read()),
                lastBatch(_) => metadata::<IZoneOutbox::lastBatchCall>(|| self.last_batch()),
                withdrawalBatchIndex(_) => metadata::<IZoneOutbox::withdrawalBatchIndexCall>(|| self.withdrawal_batch_index.read()),
                lastFinalizedTimestamp(_) => metadata::<IZoneOutbox::lastFinalizedTimestampCall>(|| self.last_finalized_timestamp.read()),
                nextWithdrawalIndex(_) => metadata::<IZoneOutbox::nextWithdrawalIndexCall>(|| self.next_withdrawal_index.read()),
                pendingWithdrawalsCount(_) => metadata::<IZoneOutbox::pendingWithdrawalsCountCall>(|| self.pending_withdrawals_count()),
                getPendingWithdrawals(_) => metadata::<IZoneOutbox::getPendingWithdrawalsCall>(|| self.get_pending_withdrawals()),
                calculateWithdrawalFee(call) => view(call, |call| self.calculate_withdrawal_fee(call.gasLimit)),
                MAX_CALLBACK_DATA_SIZE(_) => metadata::<IZoneOutbox::MAX_CALLBACK_DATA_SIZECall>(|| Ok(U256::from(MAX_CALLBACK_DATA_SIZE))),
                MAX_WITHDRAWAL_GAS_LIMIT(_) => metadata::<IZoneOutbox::MAX_WITHDRAWAL_GAS_LIMITCall>(|| Ok(MAX_WITHDRAWAL_GAS_LIMIT)),
                MAX_GAS_FEE_RATE(_) => metadata::<IZoneOutbox::MAX_GAS_FEE_RATECall>(|| Ok(MAX_GAS_FEE_RATE)),
                WITHDRAWAL_BASE_GAS(_) => metadata::<IZoneOutbox::WITHDRAWAL_BASE_GASCall>(|| Ok(WITHDRAWAL_BASE_GAS)),
                REVEAL_TO_KEY_LENGTH(_) => metadata::<IZoneOutbox::REVEAL_TO_KEY_LENGTHCall>(|| Ok(U256::from(REVEAL_TO_KEY_LENGTH))),
                AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH(_) => metadata::<IZoneOutbox::AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTHCall>(|| Ok(U256::from(AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE))),
                setTempoGasRate(call) => mutate_void(call, msg_sender, |_, call| self.set_tempo_gas_rate(call)),
                setMaxWithdrawalsPerBlock(call) => mutate_void(call, msg_sender, |_, call| self.set_max_withdrawals_per_block(call)),
                requestWithdrawal(call) => mutate_void(call, msg_sender, |sender, call| {
                    self.request_withdrawal(
                        sender,
                        tx_context::current_tx_hash().unwrap_or_default(),
                        call,
                    )
                }),
                enqueueDepositBounceBack(call) => mutate_void(call, msg_sender, |sender, call| self.enqueue_deposit_bounce_back(sender, call)),
                finalizeWithdrawalBatch(call) => mutate(call, msg_sender, |_, call| self.finalize_withdrawal_batch(call)),
            }
            ILegacyZoneOutbox::ILegacyZoneOutboxCalls {
                requestWithdrawal(call) => mutate_void(call, msg_sender, |sender, call| self.request_withdrawal(
                    sender,
                    tx_context::current_tx_hash().unwrap_or_default(),
                    call.into(),
                )),
            }
        })
    }
}
