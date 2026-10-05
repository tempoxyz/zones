//! Atomic allocation of released source inventory to permanent replenishment jobs.
//!
//! Allocation and `ZoneOutbox` enqueueing deliberately share one REVM journal. A failed policy,
//! allowance, fee, or queue check therefore leaves neither an allocation nor a withdrawal.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, keccak256};
use tempo_precompiles::storage::Handler;
use tempo_zone_contracts::{FastTransferError, FastTransferEvent, IFastTransfer};

use crate::{
    ZoneOutbox, ZoneResult,
    storage::{L1State, L1StorageReader},
};

use super::{FastTransfer, STATE_RELEASED};

/// Bound execution and storage growth for one gross-transfer replenishment job.
pub(crate) const MAX_INVENTORY_CONTRIBUTIONS: usize = 256;

impl FastTransfer {
    pub(crate) fn inventory_job(&self, job_id: B256) -> ZoneResult<IFastTransfer::InventoryJob> {
        Ok(IFastTransfer::InventoryJob {
            intentHash: self.inventory_job_hashes[job_id].read()?,
            operator: self.inventory_job_operators[job_id].read()?,
            token: self.inventory_job_tokens[job_id].read()?,
            treasury: self.inventory_job_treasuries[job_id].read()?,
            amount: self.inventory_job_amounts[job_id].read()?,
            fallbackNonce: self.inventory_job_fallback_nonces[job_id].read()?,
            withdrawalIndex: self.inventory_job_withdrawal_indexes[job_id].read()?,
            restored: self.inventory_job_restored[job_id].read()?,
        })
    }

    /// Configure the operator treasury permitted to replenish one already initialized pool.
    pub(crate) fn configure_replenishment_route_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        caller: Address,
        token: Address,
        treasury: Address,
        enabled: bool,
    ) -> ZoneResult<()> {
        if token.is_zero() || treasury.is_zero() || self.pool_operators[token].read()? != caller {
            return Err(FastTransferError::unauthorized().into());
        }
        self.check_account(l1, caller)?;
        self.check_account(l1, treasury)?;
        self.replenishment_treasury_operators[token][treasury].write(if enabled {
            caller
        } else {
            Address::ZERO
        })?;
        self.emit_event(FastTransferEvent::replenishment_route_configured(
            token, treasury, caller, enabled,
        ))?;
        Ok(())
    }

    /// Reserve exact released transfer IDs and enqueue their operator-owned inventory in the
    /// source Outbox. Identical retries return the permanent identity without moving tokens.
    pub(crate) fn allocate_inventory_and_withdraw_verified<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        outbox: &mut ZoneOutbox,
        caller: Address,
        fee_payer: Address,
        current_tx_hash: B256,
        job_id: B256,
        transfer_ids: Vec<B256>,
        token: Address,
        treasury: Address,
    ) -> ZoneResult<(u128, u64, u64)> {
        if job_id.is_zero()
            || caller.is_zero()
            || fee_payer.is_zero()
            || token.is_zero()
            || treasury.is_zero()
            || transfer_ids.is_empty()
            || transfer_ids.len() > MAX_INVENTORY_CONTRIBUTIONS
        {
            return Err(FastTransferError::invalid_inventory_job().into());
        }
        self.check_account(l1, caller)?;
        self.check_account(l1, fee_payer)?;

        let intent_hash =
            inventory_intent_hash(job_id, caller, fee_payer, token, treasury, &transfer_ids);
        let stored_hash = self.inventory_job_hashes[job_id].read()?;
        if !stored_hash.is_zero() {
            if stored_hash != intent_hash {
                return Err(FastTransferError::inventory_job_conflict().into());
            }
            return Ok((
                self.inventory_job_amounts[job_id].read()?,
                self.inventory_job_fallback_nonces[job_id].read()?,
                self.inventory_job_withdrawal_indexes[job_id].read()?,
            ));
        }

        let mut amount = 0_u128;
        for (index, transfer_id) in transfer_ids.iter().copied().enumerate() {
            if transfer_id.is_zero() || transfer_ids[..index].contains(&transfer_id) {
                return Err(FastTransferError::duplicate_inventory_contribution().into());
            }
            if self.states[transfer_id].read()? != STATE_RELEASED {
                return Err(FastTransferError::inventory_not_released(transfer_id).into());
            }
            if self.tokens[transfer_id].read()? != token
                || self.reimbursement_accounts[transfer_id].read()? != caller
                || matches!(self.recipients[transfer_id].read()?, recipient if recipient == caller || recipient == treasury)
            {
                return Err(FastTransferError::inventory_contribution_mismatch(transfer_id).into());
            }
            if !self.inventory_allocations[transfer_id].read()?.is_zero() {
                return Err(FastTransferError::inventory_already_allocated(transfer_id).into());
            }
            amount = amount
                .checked_add(self.escrow_totals[transfer_id].read()?)
                .ok_or_else(FastTransferError::arithmetic_overflow)?;
        }
        if amount == 0 {
            return Err(FastTransferError::invalid_inventory_job().into());
        }

        // These writes and the Outbox token burn/enqueue below are in one execution journal.
        self.inventory_job_hashes[job_id].write(intent_hash)?;
        self.inventory_job_operators[job_id].write(caller)?;
        self.inventory_job_tokens[job_id].write(token)?;
        self.inventory_job_treasuries[job_id].write(treasury)?;
        self.inventory_job_amounts[job_id].write(amount)?;
        self.inventory_job_contribution_counts[job_id].write(
            u32::try_from(transfer_ids.len())
                .map_err(|_| FastTransferError::invalid_inventory_job())?,
        )?;
        for (index, transfer_id) in transfer_ids.iter().copied().enumerate() {
            let index =
                u32::try_from(index).map_err(|_| FastTransferError::invalid_inventory_job())?;
            self.inventory_job_contributions[job_id][index].write(transfer_id)?;
            self.inventory_allocations[transfer_id].write(job_id)?;
        }

        let (fallback_nonce, withdrawal_index, _) = outbox.request_operator_withdrawal(
            l1,
            caller,
            fee_payer,
            current_tx_hash,
            token,
            treasury,
            amount,
            job_id,
        )?;
        if !self.inventory_jobs_by_fallback_nonce[fallback_nonce]
            .read()?
            .is_zero()
        {
            return Err(FastTransferError::inventory_job_conflict().into());
        }
        self.inventory_job_fallback_nonces[job_id].write(fallback_nonce)?;
        self.inventory_jobs_by_fallback_nonce[fallback_nonce].write(job_id)?;
        self.inventory_job_withdrawal_indexes[job_id].write(withdrawal_index)?;
        self.emit_event(FastTransferEvent::inventory_allocated(
            job_id,
            intent_hash,
            caller,
            token,
            treasury,
            amount,
            fallback_nonce,
            withdrawal_index,
        ))?;
        Ok((amount, fallback_nonce, withdrawal_index))
    }

    /// Release a bounced job's contributions only after Inbox has minted the exact amount back to
    /// the source operator. Pending bounce claims never call this method.
    pub(crate) fn restore_allocated_inventory(
        &mut self,
        fallback_nonce: u64,
        token: Address,
        recipient: Address,
        amount: u128,
    ) -> ZoneResult<Option<B256>> {
        let job_id = self.inventory_job_for_fallback_nonce(fallback_nonce)?;
        if job_id.is_zero() {
            return Ok(None);
        }
        if self.inventory_job_tokens[job_id].read()? != token
            || self.inventory_job_operators[job_id].read()? != recipient
            || self.inventory_job_amounts[job_id].read()? != amount
        {
            return Err(FastTransferError::inventory_restoration_mismatch().into());
        }
        if self.inventory_job_restored[job_id].read()? {
            return Ok(Some(job_id));
        }

        let count = self.inventory_job_contribution_counts[job_id].read()?;
        for index in 0..count {
            let transfer_id = self.inventory_job_contributions[job_id][index].read()?;
            if self.inventory_allocations[transfer_id].read()? != job_id {
                return Err(FastTransferError::inventory_restoration_mismatch().into());
            }
            self.inventory_allocations[transfer_id].delete()?;
        }
        self.inventory_job_restored[job_id].write(true)?;
        self.emit_event(FastTransferEvent::inventory_restored(job_id, amount))?;
        Ok(Some(job_id))
    }

    /// Credit native available liquidity only after Inbox has minted the real encrypted-deposit
    /// proceeds to `FAST_TRANSFER_ADDRESS` in this same execution journal.
    pub(crate) fn credit_replenishment(
        &mut self,
        job_id: B256,
        treasury: Address,
        token: Address,
        amount: u128,
    ) -> ZoneResult<Address> {
        if job_id.is_zero() || amount == 0 {
            return Err(FastTransferError::invalid_inventory_job().into());
        }
        let operator = self.replenishment_treasury_operators[token][treasury].read()?;
        if operator.is_zero() || self.pool_operators[token].read()? != operator {
            return Err(FastTransferError::unauthorized().into());
        }
        let existing = self.replenishment_credits[job_id].read()?;
        if existing != 0 {
            return Err(FastTransferError::inventory_job_conflict().into());
        }
        let balance = self.pool_balances[token]
            .read()?
            .checked_add(amount)
            .ok_or_else(FastTransferError::arithmetic_overflow)?;
        self.replenishment_credits[job_id].write(amount)?;
        self.pool_balances[token].write(balance)?;
        self.emit_event(FastTransferEvent::replenishment_credited(
            job_id, token, operator, amount,
        ))?;
        Ok(operator)
    }

    /// Preflight an encrypted destination deposit before minting. A false result makes the Inbox
    /// take its ordinary canonical bounce path, so malformed or duplicate replenishment deposits
    /// cannot strand an imported L1 queue entry.
    pub(crate) fn can_credit_replenishment(
        &self,
        job_id: B256,
        treasury: Address,
        token: Address,
        amount: u128,
    ) -> ZoneResult<bool> {
        if job_id.is_zero() || amount == 0 || self.replenishment_credits[job_id].read()? != 0 {
            return Ok(false);
        }
        let operator = self.replenishment_treasury_operators[token][treasury].read()?;
        if operator.is_zero() || self.pool_operators[token].read()? != operator {
            return Ok(false);
        }
        Ok(self.pool_balances[token]
            .read()?
            .checked_add(amount)
            .is_some())
    }

    fn inventory_job_for_fallback_nonce(&self, fallback_nonce: u64) -> ZoneResult<B256> {
        // The core registration supplies a reverse mapping so Inbox reconciliation is O(1).
        self.inventory_jobs_by_fallback_nonce[fallback_nonce]
            .read()
            .map_err(Into::into)
    }
}

fn inventory_intent_hash(
    job_id: B256,
    operator: Address,
    fee_payer: Address,
    token: Address,
    treasury: Address,
    transfer_ids: &[B256],
) -> B256 {
    let mut encoded = Vec::with_capacity(132 + transfer_ids.len() * 32);
    encoded.extend_from_slice(b"TEMPO_FAST_REPLENISHMENT_WITHDRAWAL_V1");
    encoded.extend_from_slice(job_id.as_slice());
    encoded.extend_from_slice(operator.as_slice());
    encoded.extend_from_slice(fee_payer.as_slice());
    encoded.extend_from_slice(token.as_slice());
    encoded.extend_from_slice(treasury.as_slice());
    encoded.extend_from_slice(&(transfer_ids.len() as u32).to_be_bytes());
    for transfer_id in transfer_ids {
        encoded.extend_from_slice(transfer_id.as_slice());
    }
    keccak256(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn economic_intent_commits_contribution_order_and_accounts() {
        let ids = [B256::repeat_byte(1), B256::repeat_byte(2)];
        let base = inventory_intent_hash(
            B256::repeat_byte(3),
            Address::repeat_byte(4),
            Address::repeat_byte(9),
            Address::repeat_byte(5),
            Address::repeat_byte(6),
            &ids,
        );
        let reversed = inventory_intent_hash(
            B256::repeat_byte(3),
            Address::repeat_byte(4),
            Address::repeat_byte(9),
            Address::repeat_byte(5),
            Address::repeat_byte(6),
            &[ids[1], ids[0]],
        );
        assert_ne!(base, reversed);
    }
}
