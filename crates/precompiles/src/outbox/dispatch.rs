//! ABI dispatch for the [`ZoneOutbox`] precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_precompiles::{
    DelegateCallNotAllowed, charge_input_cost, dispatch,
    storage::{Handler, StorageCtx, evm::EvmPrecompileStorageProvider},
    view,
};
use tempo_zone_contracts::IZoneOutbox;
use zone_primitives::constants::{MAX_WITHDRAWAL_GAS_LIMIT, ZONE_CONFIG_ADDRESS};

use crate::ecies::AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE;

use super::{
    MAX_CALLBACK_DATA_SIZE, MAX_GAS_FEE_RATE, REVEAL_TO_KEY_LENGTH, WITHDRAWAL_BASE_GAS,
    ZoneOutbox, ZonePortalReader,
};

alloy_sol_types::sol! {
    function requestWithdrawal(
        address token,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes data
    ) external;
}

impl ZoneOutbox {
    fn call_with_context<P: ZonePortalReader>(
        &mut self,
        provider: &P,
        current_tx_hash: B256,
        calldata: &[u8],
        msg_sender: Address,
    ) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        // Preserve the original seven-argument overload. The generated interface
        // contains the newer overload with `revealTo`, so dispatch this selector
        // explicitly and treat it as an empty reveal key.
        if tempo_precompiles::dispatch::selector_from_calldata(calldata)
            == Some(requestWithdrawalCall::SELECTOR)
        {
            let Ok(call) = requestWithdrawalCall::abi_decode_raw_validate(&calldata[4..]) else {
                return Ok(self.storage.revert_output(Bytes::new()));
            };
            return self.request_withdrawal(
                provider,
                msg_sender,
                current_tx_hash,
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
            );
        }

        dispatch!(
            calldata,
            |call| match call {
                IZoneOutbox::IZoneOutboxCalls {
                    config(call) => view(call, |_| Ok(ZONE_CONFIG_ADDRESS)),
                    tempoGasRate(call) => view(call, |_| self.tempo_gas_rate.read()),
                    maxWithdrawalsPerBlock(call) => {
                        view(call, |_| self.max_withdrawals_per_block.read())
                    },
                    lastBatch(call) => view(call, |_| self.last_batch()),
                    withdrawalBatchIndex(call) => {
                        view(call, |_| self.withdrawal_batch_index.read())
                    },
                    lastFinalizedTimestamp(call) => {
                        view(call, |_| self.last_finalized_timestamp.read())
                    },
                    nextWithdrawalIndex(call) => {
                        view(call, |_| self.next_withdrawal_index.read())
                    },
                    pendingWithdrawalsCount(call) => {
                        view(call, |_| self.pending_withdrawals_count())
                    },
                    getPendingWithdrawals(call) => {
                        view(call, |_| self.get_pending_withdrawals())
                    },
                    calculateWithdrawalFee(call) => {
                        self.calculate_withdrawal_fee(call.gasLimit)
                    },
                    MAX_CALLBACK_DATA_SIZE(call) => {
                        view(call, |_| Ok(U256::from(MAX_CALLBACK_DATA_SIZE)))
                    },
                    MAX_WITHDRAWAL_GAS_LIMIT(call) => {
                        view(call, |_| Ok(MAX_WITHDRAWAL_GAS_LIMIT))
                    },
                    MAX_GAS_FEE_RATE(call) => view(call, |_| Ok(MAX_GAS_FEE_RATE)),
                    WITHDRAWAL_BASE_GAS(call) => view(call, |_| Ok(WITHDRAWAL_BASE_GAS)),
                    REVEAL_TO_KEY_LENGTH(call) => {
                        view(call, |_| Ok(U256::from(REVEAL_TO_KEY_LENGTH)))
                    },
                    AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH(call) => {
                        view(call, |_| Ok(U256::from(AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE)))
                    },
                    setTempoGasRate(call) => {
                        self.set_tempo_gas_rate(provider, msg_sender, call)
                    },
                    setMaxWithdrawalsPerBlock(call) => {
                        self.set_max_withdrawals_per_block(provider, msg_sender, call)
                    },
                    requestWithdrawal(call) => self.request_withdrawal(
                        provider,
                        msg_sender,
                        current_tx_hash,
                        call,
                    ),
                    enqueueDepositBounceBack(call) => {
                        self.enqueue_deposit_bounce_back(msg_sender, call)
                    },
                    finalizeWithdrawalBatch(call) => {
                        self.finalize_withdrawal_batch(provider, msg_sender, call)
                    },
                }
            },
        )
    }

    /// Wrap this precompile for registration in the zone EVM.
    pub fn create<P, F>(
        provider: P,
        tx_hash: F,
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> DynPrecompile
    where
        P: ZonePortalReader + Clone + Send + Sync + 'static,
        F: for<'a> Fn(&PrecompileInput<'a>) -> B256 + Clone + Send + Sync + 'static,
    {
        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();

        DynPrecompile::new_stateful(PrecompileId::Custom("ZoneOutbox".into()), move |input| {
            if !input.is_direct_call() {
                return Ok(PrecompileOutput::revert(
                    0,
                    SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
                    input.reservoir,
                ));
            }

            let current_tx_hash = tx_hash(&input);
            let mut storage = EvmPrecompileStorageProvider::new(
                input.internals,
                input.gas,
                input.reservoir,
                spec,
                amsterdam_eip8037_enabled,
                input.is_static,
                gas_params.clone(),
            );

            StorageCtx::enter(&mut storage, || {
                Self::new().call_with_context(&provider, current_tx_hash, input.data, input.caller)
            })
        })
    }
}
