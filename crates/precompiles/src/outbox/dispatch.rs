//! ABI dispatch for the [`ZoneOutbox`] precompile.

use alloy_primitives::{Address, B256, U256};
use revm::precompile::PrecompileResult;
use tempo_precompiles::{
    charge_input_cost, dispatch, dispatch::typed::metadata as typed_metadata, storage::Handler,
};
use tempo_zone_contracts::IZoneOutbox;
use zone_primitives::constants::{MAX_WITHDRAWAL_GAS_LIMIT, ZONE_CONFIG_ADDRESS};

use crate::{
    dispatch::{metadata, mutate, mutate_void, view},
    ecies::{AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE, COMPRESSED_PUBLIC_KEY_SIZE},
    storage::{L1State, L1StorageReader},
};

use super::{MAX_CALLBACK_DATA_SIZE, WITHDRAWAL_BASE_GAS, ZoneOutbox};

impl ZoneOutbox {
    pub(crate) fn call_with_transaction<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        calldata: &[u8],
        msg_sender: Address,
        tx_hash: B256,
        fee_payer: Address,
    ) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        dispatch!(calldata, |call| match call {
            IZoneOutbox::IZoneOutboxCalls {
                config(_) => metadata::<IZoneOutbox::configCall>(|| Ok(ZONE_CONFIG_ADDRESS)),
                tempoGasRate(_) => metadata::<IZoneOutbox::tempoGasRateCall>(|| self.tempo_gas_rate.read()),
                maxWithdrawalsPerBlock(_) => metadata::<IZoneOutbox::maxWithdrawalsPerBlockCall>(|| self.max_withdrawals_per_block.read()),
                lastBatch(_) => metadata::<IZoneOutbox::lastBatchCall>(|| self.last_batch()),
                lastFinalizedTimestamp(_) => metadata::<IZoneOutbox::lastFinalizedTimestampCall>(|| self.last_finalized_timestamp.read()),
                nextWithdrawalIndex(_) => metadata::<IZoneOutbox::nextWithdrawalIndexCall>(|| self.next_withdrawal_index.read()),
                lastFallbackNonce(_) => metadata::<IZoneOutbox::lastFallbackNonceCall>(|| self.last_fallback_nonce.read()),
                pendingWithdrawalsCount(_) => typed_metadata::<IZoneOutbox::pendingWithdrawalsCountCall, _>(|| {
                    self.pending_withdrawals_count(l1, msg_sender)
                }),
                getPendingWithdrawals(_) => typed_metadata::<IZoneOutbox::getPendingWithdrawalsCall, _>(|| {
                    self.get_pending_withdrawals(l1, msg_sender)
                }),
                calculateWithdrawalFee(call) => view(call, |call| self.calculate_withdrawal_fee(call.gasLimit)),
                MAX_CALLBACK_DATA_SIZE(_) => metadata::<IZoneOutbox::MAX_CALLBACK_DATA_SIZECall>(|| Ok(U256::from(MAX_CALLBACK_DATA_SIZE))),
                MAX_WITHDRAWAL_GAS_LIMIT(_) => metadata::<IZoneOutbox::MAX_WITHDRAWAL_GAS_LIMITCall>(|| Ok(MAX_WITHDRAWAL_GAS_LIMIT)),
                WITHDRAWAL_BASE_GAS(_) => metadata::<IZoneOutbox::WITHDRAWAL_BASE_GASCall>(|| Ok(WITHDRAWAL_BASE_GAS)),
                REVEAL_TO_KEY_LENGTH(_) => metadata::<IZoneOutbox::REVEAL_TO_KEY_LENGTHCall>(|| Ok(U256::from(COMPRESSED_PUBLIC_KEY_SIZE))),
                AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH(_) => metadata::<IZoneOutbox::AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTHCall>(|| Ok(U256::from(AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE))),
                setTempoGasRate(call) => mutate_void(call, msg_sender, |sender, call| self.set_tempo_gas_rate(l1, sender, call)),
                setMaxWithdrawalsPerBlock(call) => mutate_void(call, msg_sender, |sender, call| self.set_max_withdrawals_per_block(l1, sender, call)),
                requestWithdrawal(call) => mutate_void(call, msg_sender, |sender, call| {
                    self.request_withdrawal(l1, sender, fee_payer, tx_hash, call)
                }),
                enqueueDepositBounceBack(call) => mutate_void(call, msg_sender, |sender, call| self.enqueue_deposit_bounce_back(sender, call)),
                consumeFallbackRecipient(call) => mutate(call, msg_sender, |sender, call| self.consume_fallback_recipient(sender, call.fallbackNonce)),
                finalizeWithdrawalBatch(call) => mutate(call, msg_sender, |sender, call| self.finalize_withdrawal_batch(l1, sender, call)),
            }
        })
    }
}
