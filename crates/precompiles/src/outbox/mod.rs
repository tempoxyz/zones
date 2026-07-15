//! Native `ZoneOutbox` precompile.
//!
mod dispatch;

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolValue};
use revm::interpreter::instructions::utility::IntoAddress;
use tempo_precompiles::{
    error::TempoPrecompileError,
    storage::{Handler, StorageCtx},
    tip20::{ITIP20, TIP20Token},
};
use tempo_precompiles_macros::{Storable, contract};
use tempo_zone_contracts::{ILegacyZoneOutbox, IZoneOutbox as ZoneOutboxAbi, ZoneOutboxError};
use zone_primitives::constants::{
    EMPTY_SENTINEL, MAX_WITHDRAWAL_GAS_LIMIT, PORTAL_SEQUENCER_SLOT, ZONE_INBOX_ADDRESS,
    ZONE_OUTBOX_ADDRESS,
};

use crate::{
    chaum_pedersen::recover_point,
    ecies::AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE,
    execution::{CallCheck, CallRules, ZoneCall},
};

const MAX_CALLBACK_DATA_SIZE: usize = 1024;
const MAX_GAS_FEE_RATE: u128 = 1_000_000_000_000_000_000;
const WITHDRAWAL_BASE_GAS: u64 = 50_000;
const REVEAL_TO_KEY_LENGTH: usize = 33;
const PORTAL_TOKEN_CONFIGS_SLOT: B256 = B256::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8,
]);
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

        let Some(withdrawal) = Self::decode_withdrawal(call) else {
            return CallCheck::Continue;
        };
        if withdrawal.fallbackRecipient.is_zero() {
            CallCheck::from_error(ZoneOutboxError::invalid_fallback_recipient())
        } else if withdrawal.gasLimit > MAX_WITHDRAWAL_GAS_LIMIT {
            CallCheck::from_error(ZoneOutboxError::gas_limit_too_high())
        } else if withdrawal.data.len() > MAX_CALLBACK_DATA_SIZE {
            CallCheck::from_error(ZoneOutboxError::callback_data_too_large())
        } else {
            CallCheck::Continue
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

        if let Some(withdrawal) = Self::decode_withdrawal(call) {
            let slot = keccak256((withdrawal.token, PORTAL_TOKEN_CONFIGS_SLOT).abi_encode()).into();
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
    withdrawal_batch_index: u64,
    last_batch: LastBatchStorage,
    pending_withdrawals: Vec<PendingWithdrawalStorage>,
    pending_withdrawals_head: U256,
    max_withdrawals_per_block: U256,
    withdrawals_this_block: U256,
    current_block_number: U256,
    last_finalized_timestamp: u64,
}

impl ZoneOutbox {
    /// Initializes the precompile account code.
    pub fn initialize(&mut self) -> tempo_precompiles::Result<()> {
        self.__initialize()
    }

    fn validate_gas_limit(&self, gas_limit: u64) -> crate::ZoneResult<()> {
        if gas_limit > MAX_WITHDRAWAL_GAS_LIMIT {
            return Err(ZoneOutboxError::gas_limit_too_high().into());
        }
        Ok(())
    }

    fn calculate_fee_unchecked(&self, gas_limit: u64) -> tempo_precompiles::Result<u128> {
        let gas = u128::from(WITHDRAWAL_BASE_GAS)
            .checked_add(u128::from(gas_limit))
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        gas.checked_mul(self.tempo_gas_rate.read()?)
            .ok_or_else(TempoPrecompileError::under_overflow)
    }

    fn calculate_withdrawal_fee(&self, gas_limit: u64) -> crate::ZoneResult<u128> {
        self.validate_gas_limit(gas_limit)?;
        Ok(self.calculate_fee_unchecked(gas_limit)?)
    }

    fn validate_reveal_to(&self, reveal_to: &[u8]) -> crate::ZoneResult<()> {
        if reveal_to.is_empty() {
            return Ok(());
        }
        if reveal_to.len() != REVEAL_TO_KEY_LENGTH || !matches!(reveal_to[0], 0x02 | 0x03) {
            return Err(ZoneOutboxError::invalid_reveal_to().into());
        }
        let mut x = [0u8; 32];
        x.copy_from_slice(&reveal_to[1..]);
        if recover_point(&x, reveal_to[0]).is_none() {
            return Err(ZoneOutboxError::invalid_reveal_to().into());
        }
        Ok(())
    }

    fn validate_encrypted_sender(
        &self,
        reveal_to: &[u8],
        encrypted_sender: &[u8],
    ) -> crate::ZoneResult<()> {
        let expected = if reveal_to.is_empty() {
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
        Ok(())
    }

    fn enforce_withdrawal_block_cap(&mut self) -> crate::ZoneResult<()> {
        let max = self.max_withdrawals_per_block.read()?;
        if max.is_zero() {
            return Ok(());
        }

        let block_number = U256::from(self.storage.block_number());
        if block_number != self.current_block_number.read()? {
            self.current_block_number.write(block_number)?;
            self.withdrawals_this_block.write(U256::ZERO)?;
        }

        let withdrawals = self.withdrawals_this_block.read()?;
        if withdrawals >= max {
            return Err(ZoneOutboxError::too_many_withdrawals_this_block().into());
        }
        self.withdrawals_this_block.write(
            withdrawals
                .checked_add(U256::ONE)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;
        Ok(())
    }

    fn request_withdrawal(
        &mut self,
        caller: Address,
        current_tx_hash: B256,
        call: ZoneOutboxAbi::requestWithdrawalCall,
    ) -> crate::ZoneResult<()> {
        if call.fallbackRecipient == Address::ZERO {
            return Err(ZoneOutboxError::invalid_fallback_recipient().into());
        }
        self.validate_gas_limit(call.gasLimit)?;
        if call.data.len() > MAX_CALLBACK_DATA_SIZE {
            return Err(ZoneOutboxError::callback_data_too_large().into());
        }
        self.validate_reveal_to(&call.revealTo)?;
        self.enforce_withdrawal_block_cap()?;

        let fee = self.calculate_fee_unchecked(call.gasLimit)?;
        let total_burn = call
            .amount
            .checked_add(fee)
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        if current_tx_hash.is_zero() {
            return Err(ZoneOutboxError::invalid_current_tx_hash().into());
        }

        let mut zone_token = TIP20Token::from_address(call.token)?;
        let amount = U256::from(total_burn);
        if !zone_token.transfer_from(
            ZONE_OUTBOX_ADDRESS,
            ITIP20::transferFromCall {
                from: caller,
                to: ZONE_OUTBOX_ADDRESS,
                amount,
            },
        )? {
            return Err(ZoneOutboxError::transfer_failed().into());
        }
        zone_token.burn(ZONE_OUTBOX_ADDRESS, ITIP20::burnCall { amount })?;

        self.pending_withdrawals.push(PendingWithdrawalStorage {
            token: call.token,
            sender: caller,
            tx_hash: current_tx_hash,
            to: call.to,
            amount: call.amount,
            fee,
            memo: call.memo,
            gas_limit: call.gasLimit,
            fallback_recipient: call.fallbackRecipient,
            callback_data: call.data.clone(),
            reveal_to: call.revealTo.clone(),
        })?;

        let index = self.next_withdrawal_index.read()?;
        self.next_withdrawal_index.write(
            index
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;
        self.emit_event(ZoneOutboxAbi::WithdrawalRequested {
            withdrawalIndex: index,
            sender: caller,
            token: call.token,
            to: call.to,
            amount: call.amount,
            fee,
            memo: call.memo,
            gasLimit: call.gasLimit,
            fallbackRecipient: call.fallbackRecipient,
            data: call.data,
            revealTo: call.revealTo,
        })?;
        Ok(())
    }

    fn enqueue_deposit_bounce_back(
        &mut self,
        caller: Address,
        call: ZoneOutboxAbi::enqueueDepositBounceBackCall,
    ) -> crate::ZoneResult<()> {
        if caller != ZONE_INBOX_ADDRESS {
            return Err(ZoneOutboxError::only_zone_inbox().into());
        }

        self.pending_withdrawals.push(PendingWithdrawalStorage {
            token: call.token,
            sender: Address::ZERO,
            tx_hash: B256::ZERO,
            to: call.bouncebackRecipient,
            amount: call.amount,
            fee: 0,
            memo: B256::ZERO,
            gas_limit: 0,
            fallback_recipient: Address::ZERO,
            callback_data: Bytes::new(),
            reveal_to: Bytes::new(),
        })?;

        let index = self.next_withdrawal_index.read()?;
        self.next_withdrawal_index.write(
            index
                .checked_add(1)
                .ok_or_else(TempoPrecompileError::under_overflow)?,
        )?;
        self.emit_event(ZoneOutboxAbi::WithdrawalRequested {
            withdrawalIndex: index,
            sender: Address::ZERO,
            token: call.token,
            to: call.bouncebackRecipient,
            amount: call.amount,
            fee: 0,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackRecipient: Address::ZERO,
            data: Bytes::new(),
            revealTo: Bytes::new(),
        })?;
        Ok(())
    }

    fn finalize_withdrawal_batch(
        &mut self,
        call: ZoneOutboxAbi::finalizeWithdrawalBatchCall,
    ) -> crate::ZoneResult<B256> {
        if call.blockNumber != self.storage.block_number() {
            return Err(ZoneOutboxError::invalid_block_number().into());
        }

        let len = self.pending_withdrawals.len()?;
        let head = checked_usize(self.pending_withdrawals_head.read()?)?;
        let pending = len.saturating_sub(head);
        let count = checked_usize(call.count)?;
        if count != pending {
            return Err(
                ZoneOutboxError::invalid_withdrawal_count(call.count, U256::from(pending)).into(),
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
            withdrawal_queue_hash = EMPTY_SENTINEL;
            let end = head + count;
            for i in (head..end).rev() {
                let pending_withdrawal = self.pending_withdrawals[i].read()?;
                let encrypted_sender = call.encryptedSenders[i - head].clone();
                self.validate_encrypted_sender(
                    &pending_withdrawal.reveal_to,
                    encrypted_sender.as_ref(),
                )?;
                let withdrawal = ZoneOutboxAbi::Withdrawal {
                    token: pending_withdrawal.token,
                    senderTag: sender_tag(pending_withdrawal.sender, pending_withdrawal.tx_hash),
                    to: pending_withdrawal.to,
                    amount: pending_withdrawal.amount,
                    fee: pending_withdrawal.fee,
                    memo: pending_withdrawal.memo,
                    gasLimit: pending_withdrawal.gas_limit,
                    fallbackRecipient: pending_withdrawal.fallback_recipient,
                    callbackData: pending_withdrawal.callback_data,
                    encryptedSender: encrypted_sender,
                };
                withdrawal_queue_hash = keccak256((withdrawal, withdrawal_queue_hash).abi_encode());
                self.pending_withdrawals[i].delete()?;
            }
            self.pending_withdrawals_head.write(U256::from(end))?;
            if end == len {
                self.pending_withdrawals.delete()?;
                self.pending_withdrawals_head.write(U256::ZERO)?;
            }
        }

        let next_batch_index = self
            .withdrawal_batch_index
            .read()?
            .checked_add(1)
            .ok_or_else(TempoPrecompileError::under_overflow)?;
        self.withdrawal_batch_index.write(next_batch_index)?;
        self.last_batch.write(LastBatchStorage {
            withdrawal_queue_hash,
            withdrawal_batch_index: next_batch_index,
        })?;
        self.last_finalized_timestamp
            .write(self.storage.timestamp().to::<u64>())?;
        self.emit_event(ZoneOutboxAbi::BatchFinalized {
            withdrawalQueueHash: withdrawal_queue_hash,
            withdrawalBatchIndex: next_batch_index,
        })?;
        Ok(withdrawal_queue_hash)
    }

    fn set_tempo_gas_rate(
        &mut self,
        call: ZoneOutboxAbi::setTempoGasRateCall,
    ) -> crate::ZoneResult<()> {
        if call._tempoGasRate > MAX_GAS_FEE_RATE {
            return Err(ZoneOutboxError::gas_fee_rate_too_high().into());
        }
        self.tempo_gas_rate.write(call._tempoGasRate)?;
        self.emit_event(ZoneOutboxAbi::TempoGasRateUpdated {
            tempoGasRate: call._tempoGasRate,
        })?;
        Ok(())
    }

    fn set_max_withdrawals_per_block(
        &mut self,
        call: ZoneOutboxAbi::setMaxWithdrawalsPerBlockCall,
    ) -> tempo_precompiles::Result<()> {
        self.max_withdrawals_per_block
            .write(call._maxWithdrawalsPerBlock)?;
        self.emit_event(ZoneOutboxAbi::MaxWithdrawalsPerBlockUpdated {
            maxWithdrawalsPerBlock: call._maxWithdrawalsPerBlock,
        })?;
        Ok(())
    }

    fn pending_withdrawals_count(&self) -> tempo_precompiles::Result<U256> {
        let len = self.pending_withdrawals.len()?;
        let head = checked_usize(self.pending_withdrawals_head.read()?)?;
        if head >= len {
            Ok(U256::ZERO)
        } else {
            Ok(U256::from(len - head))
        }
    }

    fn get_pending_withdrawals(
        &self,
    ) -> tempo_precompiles::Result<Vec<ZoneOutboxAbi::PendingWithdrawal>> {
        let len = self.pending_withdrawals.len()?;
        let head = checked_usize(self.pending_withdrawals_head.read()?)?;
        if head >= len {
            return Ok(Vec::new());
        }
        let mut pending = Vec::with_capacity(len - head);
        for index in head..len {
            pending.push(self.pending_withdrawals[index].read()?.into_abi());
        }
        Ok(pending)
    }

    fn last_batch(&self) -> tempo_precompiles::Result<ZoneOutboxAbi::LastBatch> {
        Ok(self.last_batch.read()?.into_abi())
    }
}

impl LastBatchStorage {
    fn into_abi(self) -> ZoneOutboxAbi::LastBatch {
        ZoneOutboxAbi::LastBatch {
            withdrawalQueueHash: self.withdrawal_queue_hash,
            withdrawalBatchIndex: self.withdrawal_batch_index,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Storable)]
struct LastBatchStorage {
    withdrawal_queue_hash: B256,
    withdrawal_batch_index: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Storable)]
struct PendingWithdrawalStorage {
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

impl PendingWithdrawalStorage {
    fn into_abi(self) -> ZoneOutboxAbi::PendingWithdrawal {
        ZoneOutboxAbi::PendingWithdrawal {
            token: self.token,
            sender: self.sender,
            txHash: self.tx_hash,
            to: self.to,
            amount: self.amount,
            fee: self.fee,
            memo: self.memo,
            gasLimit: self.gas_limit,
            fallbackRecipient: self.fallback_recipient,
            callbackData: self.callback_data,
            revealTo: self.reveal_to,
        }
    }
}

fn checked_usize(value: U256) -> tempo_precompiles::Result<usize> {
    if value > U256::from(u32::MAX) {
        return Err(TempoPrecompileError::under_overflow());
    }
    Ok(value.to::<usize>())
}

fn sender_tag(sender: Address, tx_hash: B256) -> B256 {
    let mut preimage = [0u8; 52];
    preimage[..20].copy_from_slice(sender.as_slice());
    preimage[20..].copy_from_slice(tx_hash.as_slice());
    keccak256(preimage)
}
