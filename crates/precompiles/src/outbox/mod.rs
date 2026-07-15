//! Native `ZoneOutbox` precompile.
//!
mod dispatch;
#[cfg(test)]
mod tests;

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use revm::interpreter::instructions::utility::IntoAddress;
use tempo_precompiles::{
    Result as TempoResult,
    error::TempoPrecompileError,
    storage::{Handler, StorageCtx},
    tip20::{ITIP20, TIP20Token},
};
use tempo_precompiles_macros::{Storable, contract};
use tempo_zone_contracts::{
    ILegacyZoneOutbox, IZoneOutbox as ZoneOutboxAbi, Withdrawal, ZoneOutboxError, ZoneOutboxEvent,
    portal_token_config_slot,
};
use zone_primitives::constants::{
    MAX_WITHDRAWAL_GAS_LIMIT, PORTAL_SEQUENCER_SLOT, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
};

use crate::{
    ZoneResult,
    ecies::{AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE, decode_compressed_public_key},
    execution::{CallCheck, CallRules, ZoneCall},
};

const MAX_CALLBACK_DATA_SIZE: usize = 1024;
const MAX_GAS_FEE_RATE: u128 = 1_000_000_000_000_000_000;
const WITHDRAWAL_BASE_GAS: u64 = 50_000;

const SEQUENCER_SELECTORS: &[[u8; 4]] = &[
    ZoneOutboxAbi::setTempoGasRateCall::SELECTOR,
    ZoneOutboxAbi::setMaxWithdrawalsPerBlockCall::SELECTOR,
    ZoneOutboxAbi::finalizeWithdrawalBatchCall::SELECTOR,
];
const WITHDRAWAL_SELECTORS: &[[u8; 4]] = &[
    ZoneOutboxAbi::requestWithdrawalCall::SELECTOR,
    ILegacyZoneOutbox::requestWithdrawalCall::SELECTOR,
];

/// Admission checks that require the finalized `ZonePortal` state.
pub(crate) struct ZoneOutboxRules {
    portal: Address,
}

impl ZoneOutboxRules {
    pub(crate) fn new(portal: Address) -> Self {
        Self { portal }
    }
}

impl CallRules for ZoneOutboxRules {
    fn requires_l1(&self, selector: Option<[u8; 4]>) -> bool {
        selector.is_some_and(|selector| {
            SEQUENCER_SELECTORS.contains(&selector) || WITHDRAWAL_SELECTORS.contains(&selector)
        })
    }

    fn check_with_local_state(&self, call: ZoneCall<'_>) -> CallCheck {
        if call.is_static
            && call.selector().is_some_and(|selector| {
                self.requires_l1(Some(selector))
                    || selector == ZoneOutboxAbi::enqueueDepositBounceBackCall::SELECTOR
            })
        {
            return CallCheck::from_error(ZoneOutboxError::static_call_not_allowed());
        }

        let Some(withdrawal) = decode_withdrawal(call) else {
            return CallCheck::Continue;
        };
        match check_withdrawal_request(&withdrawal) {
            Ok(()) => CallCheck::Continue,
            Err(err) => CallCheck::from_error(err),
        }
    }

    fn check_with_l1_backed_state(&self, call: ZoneCall<'_>) -> CallCheck {
        let Some(selector) = call.selector() else {
            return CallCheck::Continue;
        };
        if SEQUENCER_SELECTORS.contains(&selector) && call.caller != Address::ZERO {
            let sequencer = match StorageCtx::default()
                .sload(self.portal, U256::from_be_bytes(PORTAL_SEQUENCER_SLOT.0))
            {
                Ok(value) => value.into_address(),
                Err(err) => return CallCheck::from_error(err),
            };
            if sequencer != call.caller {
                return CallCheck::from_error(ZoneOutboxError::only_sequencer());
            }
        }

        if let Some(withdrawal) = decode_withdrawal(call) {
            let slot = portal_token_config_slot(withdrawal.token).into();
            match StorageCtx::default().sload(self.portal, slot) {
                Ok(value) if value.byte(0) != 0 => {}
                Ok(_) => return CallCheck::from_error(ZoneOutboxError::token_not_enabled()),
                Err(err) => return CallCheck::from_error(err),
            }
        }
        CallCheck::Continue
    }
}

#[contract(addr = ZONE_OUTBOX_ADDRESS)]
pub struct ZoneOutbox {
    tempo_gas_rate: u128,
    next_withdrawal_index: u64,
    withdrawal_queue_hash: B256,
    withdrawal_batch_index: u64,
    max_withdrawals_per_block: u32,
    withdrawals_this_block: u32,
    current_block_number: u64,
    last_finalized_timestamp: u64,
    pending_withdrawals: Vec<PendingWithdrawal>,
}

impl ZoneOutbox {
    /// Initializes the precompile account code.
    pub fn initialize(&mut self) -> TempoResult<()> {
        self.__initialize()
    }

    fn calculate_fee_unchecked(&self, gas_limit: u64) -> TempoResult<u128> {
        let gas = u128::from(WITHDRAWAL_BASE_GAS) + u128::from(gas_limit);
        gas.checked_mul(self.tempo_gas_rate.read()?)
            .ok_or_else(TempoPrecompileError::under_overflow)
    }

    fn calculate_withdrawal_fee(&self, gas_limit: u64) -> ZoneResult<u128> {
        validate_gas_limit(gas_limit)?;
        self.calculate_fee_unchecked(gas_limit).map_err(Into::into)
    }

    fn enforce_withdrawal_block_cap(&mut self) -> ZoneResult<()> {
        let max = self.max_withdrawals_per_block.read()?;
        if max == 0 {
            return Ok(());
        }

        let block_number = self.storage.block_number();
        if block_number != self.current_block_number.read()? {
            self.current_block_number.write(block_number)?;
            self.withdrawals_this_block.write(0)?;
        }

        let withdrawals = self.withdrawals_this_block.read()?;
        if withdrawals >= max {
            return Err(ZoneOutboxError::too_many_withdrawals_this_block().into());
        }
        self.withdrawals_this_block.write(
            withdrawals
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;
        Ok(())
    }

    fn enqueue(&mut self, pending: PendingWithdrawal) -> ZoneResult<()> {
        let index = self.next_withdrawal_index.read()?;
        self.emit_event(pending.requested_event(index))?;

        self.pending_withdrawals.push(pending)?;
        self.next_withdrawal_index.write(
            index
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;
        Ok(())
    }

    fn request_withdrawal(
        &mut self,
        caller: Address,
        current_tx_hash: B256,
        call: ZoneOutboxAbi::requestWithdrawalCall,
    ) -> ZoneResult<()> {
        if current_tx_hash.is_zero() {
            return Err(ZoneOutboxError::invalid_current_tx_hash().into());
        }
        check_withdrawal_request(&call)?;
        self.enforce_withdrawal_block_cap()?;

        // If necessary, validate reveal
        if !call.revealTo.is_empty() && decode_compressed_public_key(&call.revealTo).is_none() {
            return Err(ZoneOutboxError::invalid_reveal_to().into());
        }

        let fee = self.calculate_fee_unchecked(call.gasLimit)?;
        let total_burn = call
            .amount
            .checked_add(fee)
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        let mut zone_token = TIP20Token::from_address(call.token)?;
        let amount = U256::from(total_burn);
        if !zone_token.transfer_from(
            self.address,
            ITIP20::transferFromCall {
                from: caller,
                to: self.address,
                amount,
            },
        )? {
            return Err(ZoneOutboxError::transfer_failed().into());
        }
        zone_token.burn(self.address, ITIP20::burnCall { amount })?;

        self.enqueue(PendingWithdrawal::from_request(
            caller,
            current_tx_hash,
            fee,
            call,
        ))
    }

    fn enqueue_deposit_bounce_back(
        &mut self,
        caller: Address,
        call: ZoneOutboxAbi::enqueueDepositBounceBackCall,
    ) -> ZoneResult<()> {
        if caller != ZONE_INBOX_ADDRESS {
            return Err(ZoneOutboxError::only_zone_inbox().into());
        }

        self.enqueue(PendingWithdrawal::from_bounce_back(call))
    }

    fn finalize_withdrawal_batch(
        &mut self,
        call: ZoneOutboxAbi::finalizeWithdrawalBatchCall,
    ) -> ZoneResult<B256> {
        if call.blockNumber != self.storage.block_number() {
            return Err(ZoneOutboxError::invalid_block_number().into());
        }

        let count = self.pending_withdrawals.len()?;
        if call.count != U256::from(count) {
            return Err(
                ZoneOutboxError::invalid_withdrawal_count(call.count, U256::from(count)).into(),
            );
        }
        if call.encryptedSenders.len() != count {
            return Err(ZoneOutboxError::invalid_encrypted_sender_count(
                U256::from(call.encryptedSenders.len()),
                U256::from(count),
            )
            .into());
        }

        let mut withdrawal_queue_hash = B256::ZERO;
        if count > 0 {
            withdrawal_queue_hash = zone_primitives::constants::EMPTY_SENTINEL;
            for (index, encrypted_sender) in call.encryptedSenders.into_iter().enumerate().rev() {
                let pending = self.pending_withdrawals[index].read()?;
                let withdrawal = pending.into_withdrawal(encrypted_sender)?;
                withdrawal_queue_hash = withdrawal.hash_with_tail(withdrawal_queue_hash);
            }
            self.pending_withdrawals.delete()?;
        }

        let next_batch_index = self
            .withdrawal_batch_index
            .read()?
            .checked_add(1)
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        self.withdrawal_batch_index.write(next_batch_index)?;
        self.withdrawal_queue_hash.write(withdrawal_queue_hash)?;
        self.last_finalized_timestamp
            .write(self.storage.timestamp().to::<u64>())?;
        self.emit_event(ZoneOutboxEvent::batch_finalized(
            withdrawal_queue_hash,
            next_batch_index,
        ))?;
        Ok(withdrawal_queue_hash)
    }

    fn set_tempo_gas_rate(&mut self, call: ZoneOutboxAbi::setTempoGasRateCall) -> ZoneResult<()> {
        if call._tempoGasRate > MAX_GAS_FEE_RATE {
            return Err(ZoneOutboxError::gas_fee_rate_too_high().into());
        }
        self.tempo_gas_rate.write(call._tempoGasRate)?;
        self.emit_event(ZoneOutboxEvent::tempo_gas_rate_updated(call._tempoGasRate))?;
        Ok(())
    }

    fn set_max_withdrawals_per_block(
        &mut self,
        call: ZoneOutboxAbi::setMaxWithdrawalsPerBlockCall,
    ) -> TempoResult<()> {
        self.max_withdrawals_per_block
            .write(call._maxWithdrawalsPerBlock)?;
        self.emit_event(ZoneOutboxEvent::max_withdrawals_per_block_updated(
            call._maxWithdrawalsPerBlock,
        ))?;
        Ok(())
    }

    fn pending_withdrawals_count(&self) -> TempoResult<U256> {
        self.pending_withdrawals.len().map(|val| U256::from(val))
    }

    fn get_pending_withdrawals(&self) -> TempoResult<Vec<ZoneOutboxAbi::PendingWithdrawal>> {
        let len = self.pending_withdrawals.len()?;
        let mut pending = Vec::with_capacity(len);
        for index in 0..len {
            pending.push(self.pending_withdrawals[index].read()?.into());
        }
        Ok(pending)
    }

    fn last_batch(&self) -> TempoResult<ZoneOutboxAbi::LastBatch> {
        Ok(ZoneOutboxAbi::LastBatch {
            withdrawalQueueHash: self.withdrawal_queue_hash.read()?,
            withdrawalBatchIndex: self.withdrawal_batch_index.read()?,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Storable)]
struct PendingWithdrawal {
    token: Address,
    sender: Address,
    tx_hash: B256,
    to: Address,
    amount: u128,
    fee: u128,
    memo: B256,
    gas_limit: u64,
    fallback_recipient: Address,
    callback_data: Bytes,
    reveal_to: Bytes,
}

impl PendingWithdrawal {
    fn from_request(
        sender: Address,
        tx_hash: B256,
        fee: u128,
        call: ZoneOutboxAbi::requestWithdrawalCall,
    ) -> Self {
        Self {
            token: call.token,
            sender,
            tx_hash,
            to: call.to,
            amount: call.amount,
            fee,
            memo: call.memo,
            gas_limit: call.gasLimit,
            fallback_recipient: call.fallbackRecipient,
            callback_data: call.data,
            reveal_to: call.revealTo,
        }
    }

    fn from_bounce_back(call: ZoneOutboxAbi::enqueueDepositBounceBackCall) -> Self {
        Self {
            token: call.token,
            to: call.bouncebackRecipient,
            amount: call.amount,
            ..Default::default()
        }
    }

    fn requested_event(&self, index: u64) -> ZoneOutboxEvent {
        ZoneOutboxEvent::withdrawal_requested(
            index,
            self.sender,
            self.token,
            self.to,
            self.amount,
            self.fee,
            self.memo,
            self.gas_limit,
            self.fallback_recipient,
            self.callback_data.clone(),
            self.reveal_to.clone(),
        )
    }

    fn into_withdrawal(self, encrypted_sender: Bytes) -> ZoneResult<Withdrawal> {
        let expected = if self.reveal_to.is_empty() {
            0
        } else {
            AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE
        };
        if encrypted_sender.len() != expected {
            return Err(ZoneOutboxError::invalid_encrypted_sender_length(
                U256::from(encrypted_sender.len()),
                U256::from(expected),
            )
            .into());
        }

        let sender_tag = Withdrawal::sender_tag(self.sender, self.tx_hash);
        Ok(Withdrawal {
            token: self.token,
            senderTag: sender_tag,
            to: self.to,
            amount: self.amount,
            fee: self.fee,
            memo: self.memo,
            gasLimit: self.gas_limit,
            fallbackRecipient: self.fallback_recipient,
            callbackData: self.callback_data,
            encryptedSender: encrypted_sender,
        })
    }
}

impl From<PendingWithdrawal> for ZoneOutboxAbi::PendingWithdrawal {
    fn from(pending: PendingWithdrawal) -> Self {
        Self {
            token: pending.token,
            sender: pending.sender,
            txHash: pending.tx_hash,
            to: pending.to,
            amount: pending.amount,
            fee: pending.fee,
            memo: pending.memo,
            gasLimit: pending.gas_limit,
            fallbackRecipient: pending.fallback_recipient,
            callbackData: pending.callback_data,
            revealTo: pending.reveal_to,
        }
    }
}

fn decode_withdrawal(call: ZoneCall<'_>) -> Option<ZoneOutboxAbi::requestWithdrawalCall> {
    match call.selector()? {
        ZoneOutboxAbi::requestWithdrawalCall::SELECTOR => {
            ZoneOutboxAbi::requestWithdrawalCall::abi_decode_raw_validate(&call.data[4..]).ok()
        }
        ILegacyZoneOutbox::requestWithdrawalCall::SELECTOR => {
            ILegacyZoneOutbox::requestWithdrawalCall::abi_decode_raw_validate(&call.data[4..])
                .ok()
                .map(Into::into)
        }
        _ => None,
    }
}

fn validate_gas_limit(gas_limit: u64) -> ZoneResult<()> {
    if gas_limit > MAX_WITHDRAWAL_GAS_LIMIT {
        return Err(ZoneOutboxError::gas_limit_too_high().into());
    }
    Ok(())
}

fn check_withdrawal_request(call: &ZoneOutboxAbi::requestWithdrawalCall) -> ZoneResult<()> {
    if call.fallbackRecipient.is_zero() {
        return Err(ZoneOutboxError::invalid_fallback_recipient().into());
    }
    validate_gas_limit(call.gasLimit)?;
    if call.data.len() > MAX_CALLBACK_DATA_SIZE {
        return Err(ZoneOutboxError::callback_data_too_large().into());
    }
    Ok(())
}
