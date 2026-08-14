//! Batch and withdrawal-queue validation.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{B256, U256};

use super::{
    Cursor, InvariantCode, InvariantViolation, PortalIdentity, Settlement, State, ZoneState,
    is_cursor_prefix, is_well_formed_cursor, violation,
};
use crate::kernel::state::{BatchBoundary, BatchState, StateKey, StateValue, WithdrawalOwner};

/// Validate finalized and submitted batch ownership, linkage, and ring order.
pub(super) fn validate(
    state: &State,
    identity: PortalIdentity,
    zone: &ZoneState,
    settlement: &Settlement,
) -> Result<(), InvariantViolation> {
    BatchValidator::new(state, identity, zone, settlement)?.validate()
}

/// Batch fields shared by finalized and submitted records.
struct BatchView {
    boundary: BatchBoundary,
    first_withdrawal: u64,
    withdrawal_count: u64,
    queue_hash: B256,
    next_ordinal: u64,
    logical_queue_index: Option<U256>,
}

/// The final boundary reached by a preceding batch sequence.
#[derive(Clone, Copy)]
struct BatchProgress {
    index: u64,
    final_block: B256,
    final_deposit: Cursor,
    zone_height: u64,
    tempo_block: u64,
}

/// Stateful validation of finalized and submitted batch sequences.
struct BatchValidator<'a> {
    state: &'a State,
    identity: PortalIdentity,
    zone: &'a ZoneState,
    settlement: &'a Settlement,
    queues: BTreeMap<U256, u64>,
    owned_withdrawals: BTreeSet<u64>,
    prior_end: Option<u64>,
    submitted_tip: Option<BatchProgress>,
    finalized_tip: BatchProgress,
    last_end: Option<u64>,
}

impl<'a> BatchValidator<'a> {
    /// Initialize validation from the current settlement boundary.
    fn new(
        state: &'a State,
        identity: PortalIdentity,
        zone: &'a ZoneState,
        settlement: &'a Settlement,
    ) -> Result<Self, InvariantViolation> {
        let finalized_tip = BatchProgress {
            index: settlement.batch_index,
            final_block: settlement.block_hash,
            final_deposit: settlement.submitted_deposit,
            zone_height: u64::try_from(settlement.zone_height)
                .map_err(|_| violation(InvariantCode::Batch, Some(StateKey::Portal)))?,
            tempo_block: settlement.tempo_block,
        };
        Ok(Self {
            state,
            identity,
            zone,
            settlement,
            queues: BTreeMap::new(),
            owned_withdrawals: BTreeSet::new(),
            prior_end: None,
            submitted_tip: None,
            finalized_tip,
            last_end: None,
        })
    }

    /// Validate every persisted batch, then reconcile the resulting tips and queue.
    fn validate(&mut self) -> Result<(), InvariantViolation> {
        for (key, value) in self.state.rows() {
            self.validate_row(*key, value)?;
        }
        self.validate_terminal_state()
    }

    /// Validate one batch row and advance the corresponding sequence tip.
    fn validate_row(
        &mut self,
        key: StateKey,
        value: &StateValue,
    ) -> Result<(), InvariantViolation> {
        let (StateKey::Batch(id), StateValue::Batch(batch)) = (key, value) else {
            return Ok(());
        };
        let index = id.index.get();
        let batch = self.decode_batch(key, index, batch)?;
        let end = self.validate_bounds(key, id.zone_id, index, &batch)?;
        self.record_range_end(end);
        self.advance_sequence(key, index, &batch)?;
        self.validate_withdrawal_range(key, &batch, end)?;
        self.validate_batch_tip(key, index, &batch, end)
    }

    /// Normalize a persisted batch and register its submitted queue slot.
    fn decode_batch(
        &mut self,
        key: StateKey,
        index: u64,
        batch: &BatchState,
    ) -> Result<BatchView, InvariantViolation> {
        Ok(match batch {
            BatchState::Finalized {
                boundary,
                first_withdrawal,
                count,
                queue_hash,
            } => {
                if index <= self.settlement.batch_index {
                    return Err(self.batch_error(key));
                }
                BatchView {
                    boundary: *boundary,
                    first_withdrawal: *first_withdrawal,
                    withdrawal_count: *count,
                    queue_hash: *queue_hash,
                    next_ordinal: 0,
                    logical_queue_index: None,
                }
            }
            BatchState::Submitted {
                boundary,
                first_withdrawal,
                count,
                queue_hash,
                next_ordinal,
                logical_queue_index,
            } => {
                if index > self.settlement.batch_index
                    || *logical_queue_index < self.settlement.queue_head
                    || *logical_queue_index >= self.settlement.queue_tail
                    || self.queues.insert(*logical_queue_index, index).is_some()
                {
                    return Err(violation(InvariantCode::Ring, Some(key)));
                }
                BatchView {
                    boundary: *boundary,
                    first_withdrawal: *first_withdrawal,
                    withdrawal_count: *count,
                    queue_hash: *queue_hash,
                    next_ordinal: *next_ordinal,
                    logical_queue_index: Some(*logical_queue_index),
                }
            }
        })
    }

    /// Validate one batch's identity, cursor bounds, and withdrawal contiguity.
    fn validate_bounds(
        &self,
        key: StateKey,
        zone_id: u32,
        index: u64,
        batch: &BatchView,
    ) -> Result<u64, InvariantViolation> {
        let end = batch
            .first_withdrawal
            .checked_add(batch.withdrawal_count)
            .ok_or_else(|| self.batch_error(key))?;
        if zone_id != self.identity.zone_id
            || index > self.zone.withdrawal_batch_index
            || batch.next_ordinal > batch.withdrawal_count
            || end > self.zone.next_withdrawal_index
            || !is_well_formed_cursor(batch.boundary.first_deposit)
            || !is_well_formed_cursor(batch.boundary.final_deposit)
            || !is_cursor_prefix(batch.boundary.first_deposit, batch.boundary.final_deposit)
            || !is_cursor_prefix(batch.boundary.final_deposit, self.zone.processed_deposit)
            || self
                .prior_end
                .or((self.settlement.batch_index == 0).then_some(0))
                .is_some_and(|prior| prior != batch.first_withdrawal)
        {
            return Err(self.batch_error(key));
        }
        Ok(end)
    }

    /// Record the contiguous withdrawal range validated for a batch.
    fn record_range_end(&mut self, end: u64) {
        self.prior_end = Some(end);
        self.last_end = Some(end);
    }

    /// Advance the submitted or finalized sequence represented by a batch.
    fn advance_sequence(
        &mut self,
        key: StateKey,
        index: u64,
        batch: &BatchView,
    ) -> Result<(), InvariantViolation> {
        match batch.logical_queue_index {
            Some(queue_index) => self.advance_submitted_sequence(key, index, batch, queue_index),
            None => self.advance_finalized_sequence(key, index, batch),
        }
    }

    /// Advance the submitted-batch sequence and retain its tip.
    fn advance_submitted_sequence(
        &mut self,
        key: StateKey,
        index: u64,
        batch: &BatchView,
        queue_index: U256,
    ) -> Result<(), InvariantViolation> {
        if queue_index != self.settlement.queue_head && batch.next_ordinal != 0 {
            return Err(self.batch_error(key));
        }
        if let Some(previous) = self.submitted_tip {
            let advance = index - previous.index;
            let adjacent = advance == 1;
            let deposit_bad =
                !is_cursor_prefix(previous.final_deposit, batch.boundary.first_deposit)
                    || (adjacent && batch.boundary.first_deposit != previous.final_deposit);
            let zone_advance = batch
                .boundary
                .zone_height
                .saturating_sub(previous.zone_height);
            let tempo_advance = batch
                .boundary
                .tempo_block
                .saturating_sub(previous.tempo_block);
            if (adjacent && batch.boundary.first_parent != previous.final_block)
                || deposit_bad
                || zone_advance < advance
                || tempo_advance < zone_advance
            {
                return Err(self.batch_error(key));
            }
        }
        self.submitted_tip = Some(BatchProgress {
            index,
            final_block: batch.boundary.final_block,
            final_deposit: batch.boundary.final_deposit,
            zone_height: batch.boundary.zone_height,
            tempo_block: batch.boundary.tempo_block,
        });
        Ok(())
    }

    /// Advance the finalized-batch sequence and retain its tip.
    fn advance_finalized_sequence(
        &mut self,
        key: StateKey,
        index: u64,
        batch: &BatchView,
    ) -> Result<(), InvariantViolation> {
        let zone_advance = batch
            .boundary
            .zone_height
            .checked_sub(self.finalized_tip.zone_height)
            .filter(|advance| *advance != 0);
        let tempo_advance = batch
            .boundary
            .tempo_block
            .checked_sub(self.finalized_tip.tempo_block)
            .filter(|advance| *advance != 0);
        if index
            != self
                .finalized_tip
                .index
                .checked_add(1)
                .ok_or_else(|| self.batch_error(key))?
            || batch.boundary.first_parent != self.finalized_tip.final_block
            || batch.boundary.first_deposit != self.finalized_tip.final_deposit
            || zone_advance.is_none()
            || tempo_advance.is_none()
            || (self.finalized_tip.index != 0 && zone_advance > tempo_advance)
        {
            return Err(self.batch_error(key));
        }
        self.finalized_tip = BatchProgress {
            index,
            final_block: batch.boundary.final_block,
            final_deposit: batch.boundary.final_deposit,
            zone_height: batch.boundary.zone_height,
            tempo_block: batch.boundary.tempo_block,
        };
        Ok(())
    }

    /// Require the batch's finalized withdrawal range to be unique and hash-consistent.
    fn validate_withdrawal_range(
        &mut self,
        key: StateKey,
        batch: &BatchView,
        end: u64,
    ) -> Result<(), InvariantViolation> {
        let start = batch
            .first_withdrawal
            .checked_add(batch.next_ordinal)
            .ok_or_else(|| self.batch_error(key))?;
        let mut values = Vec::new();
        for withdrawal_index in start..end {
            let withdrawal_key = StateKey::Withdrawal(crate::kernel::state::WithdrawalId {
                zone_id: self.identity.zone_id,
                index: withdrawal_index,
            });
            let Some(StateValue::Withdrawal(WithdrawalOwner::Finalized { data, .. })) =
                self.state.rows().get(&withdrawal_key)
            else {
                return Err(self.batch_error(withdrawal_key));
            };
            if !self.owned_withdrawals.insert(withdrawal_index) {
                return Err(self.batch_error(withdrawal_key));
            }
            values.push(data.clone());
        }
        if crate::kernel::derivation::withdrawal_queue_hash(&values) != batch.queue_hash {
            return Err(self.batch_error(key));
        }

        Ok(())
    }

    /// Reconcile a batch at the settlement or Zone tip with that tip's stored fields.
    fn validate_batch_tip(
        &self,
        key: StateKey,
        index: u64,
        batch: &BatchView,
        end: u64,
    ) -> Result<(), InvariantViolation> {
        if index == self.settlement.batch_index
            && batch.logical_queue_index.is_some()
            && (self.settlement.block_hash != batch.boundary.final_block
                || self.settlement.tempo_block != batch.boundary.tempo_block
                || self.settlement.submitted_deposit != batch.boundary.final_deposit
                || self.settlement.zone_height != U256::from(batch.boundary.zone_height))
        {
            return Err(self.batch_error(key));
        }
        if index == self.zone.withdrawal_batch_index
            && (self.zone.withdrawal_queue_hash != batch.queue_hash
                || self.zone.batch_start.parent_hash != batch.boundary.final_block
                || self.zone.batch_start.deposit != batch.boundary.final_deposit
                || self.zone.batch_start.withdrawal_index != end)
        {
            return Err(self.batch_error(key));
        }
        Ok(())
    }

    /// Reconcile batch tips, finalized withdrawals, and queue slots with state counters.
    fn validate_terminal_state(&self) -> Result<(), InvariantViolation> {
        self.validate_finalized_tip()?;
        self.validate_owned_withdrawals()?;
        self.validate_queue_slots()?;
        self.validate_submitted_tip()?;
        self.validate_shared_tip()
    }

    /// Require the finalized sequence to reach the Zone batch tip.
    fn validate_finalized_tip(&self) -> Result<(), InvariantViolation> {
        if self.finalized_tip.index != self.zone.withdrawal_batch_index {
            return Err(self.batch_error(StateKey::Zone));
        }
        if self
            .last_end
            .is_some_and(|end| end != self.zone.batch_start.withdrawal_index)
        {
            return Err(self.batch_error(StateKey::Zone));
        }
        Ok(())
    }

    /// Require every finalized withdrawal to belong to exactly one batch.
    fn validate_owned_withdrawals(&self) -> Result<(), InvariantViolation> {
        for (key, value) in self.state.rows() {
            if let (
                StateKey::Withdrawal(id),
                StateValue::Withdrawal(WithdrawalOwner::Finalized { .. }),
            ) = (key, value)
                && !self.owned_withdrawals.contains(&id.index)
            {
                return Err(self.batch_error(*key));
            }
        }
        Ok(())
    }

    /// Require submitted batches to occupy the settlement ring contiguously.
    fn validate_queue_slots(&self) -> Result<(), InvariantViolation> {
        let expected = usize::try_from(self.settlement.queue_tail - self.settlement.queue_head)
            .map_err(|_| violation(InvariantCode::Ring, Some(StateKey::Portal)))?;
        if self.queues.len() != expected {
            return Err(violation(InvariantCode::Ring, Some(StateKey::Portal)));
        }
        let mut prior_batch = None;
        for (offset, (queue_index, batch_index)) in self.queues.iter().enumerate() {
            if *queue_index != self.settlement.queue_head + U256::from(offset)
                || prior_batch.is_some_and(|prior| *batch_index <= prior)
            {
                return Err(violation(InvariantCode::Ring, Some(StateKey::Portal)));
            }
            prior_batch = Some(*batch_index);
        }
        Ok(())
    }

    /// Require the submitted sequence to reach the settlement tip.
    fn validate_submitted_tip(&self) -> Result<(), InvariantViolation> {
        if let Some(previous) = self.submitted_tip
            && previous.index != self.settlement.batch_index
        {
            let advance = self.settlement.batch_index - previous.index;
            let zone_advance = u64::try_from(self.settlement.zone_height)
                .map_err(|_| self.batch_error(StateKey::Portal))?
                .saturating_sub(previous.zone_height);
            let tempo_advance = self
                .settlement
                .tempo_block
                .saturating_sub(previous.tempo_block);
            let deposit_bad =
                !is_cursor_prefix(previous.final_deposit, self.settlement.submitted_deposit);
            if deposit_bad || zone_advance < advance || tempo_advance < zone_advance {
                return Err(self.batch_error(StateKey::Portal));
            }
        }
        Ok(())
    }

    /// Require equal settlement and Zone batch tips to agree on their boundary.
    fn validate_shared_tip(&self) -> Result<(), InvariantViolation> {
        if self.settlement.batch_index == self.zone.withdrawal_batch_index
            && (self.settlement.block_hash != self.zone.batch_start.parent_hash
                || self.settlement.submitted_deposit != self.zone.batch_start.deposit)
        {
            return Err(self.batch_error(StateKey::Portal));
        }
        Ok(())
    }

    /// Construct a batch-category violation for one state row.
    fn batch_error(&self, key: StateKey) -> InvariantViolation {
        violation(InvariantCode::Batch, Some(key))
    }
}
