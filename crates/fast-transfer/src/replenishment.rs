//! Durable operator-owned two-leg replenishment records.

use std::collections::HashSet;

use alloy_primitives::{Address, B256, U256};

/// One released source inventory contribution. IDs are permanent replay keys.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InventoryContribution {
    /// Fast transfer whose released inventory is being moved.
    pub transfer_id: B256,
    /// Gross source inventory allocated from that transfer.
    pub amount: U256,
}

/// Durable two-leg job stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplenishmentStage {
    /// Released inventory was atomically reserved in the source Zone.
    InventoryAllocated,
    /// Source withdrawal economic action was submitted or is being reconciled.
    WithdrawalRequested,
    /// Withdrawal bounced; wait for actual fallback inventory restoration.
    WithdrawalBounced,
    /// L1 treasury has a finalized observed credit.
    TreasuryFunded,
    /// Destination encrypted deposit was submitted or is being reconciled.
    DepositSubmitted,
    /// Deposit failed; wait for actual treasury refund or claimable refund.
    DepositRefundPending,
    /// Destination pool has an observed real token credit.
    PoolCredited,
}

/// Persisted source withdrawal identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawalRecord {
    /// Exact pre-submission transaction intent hash.
    pub transaction_intent_hash: B256,
    /// Signer nonce reused by replacements.
    pub signer_nonce: u64,
    /// Source fallback nonce/identity.
    pub fallback_nonce: u64,
    /// Source withdrawal transaction hash, if observed.
    pub transaction_hash: Option<B256>,
    /// Accepted source batch hash, if finalized.
    pub accepted_batch_hash: Option<B256>,
    /// Sequential withdrawal queue identity, if assigned.
    pub queue_index: Option<u64>,
}

/// Persisted destination deposit identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositRecord {
    /// Exact pre-submission encrypted-deposit intent hash.
    pub transaction_intent_hash: B256,
    /// L1 treasury signer nonce reused by replacements.
    pub signer_nonce: u64,
    /// Destination deposit queue identity, if assigned.
    pub queue_index: Option<u64>,
    /// L1 transaction hash, if observed.
    pub transaction_hash: Option<B256>,
}

/// Replenishment transition failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ReplenishmentError {
    /// Job has no inventory or duplicates a contribution ID.
    #[error("invalid inventory contributions")]
    InvalidContributions,
    /// Amount arithmetic overflowed or evidence exceeded the job amount.
    #[error("invalid replenishment amount")]
    InvalidAmount,
    /// A transition skipped a required verified effect or conflicted with prior state.
    #[error("replenishment transition is out of order")]
    OutOfOrder,
    /// Replacement tried to change nonce or economic action.
    #[error("replacement changes the persisted economic action")]
    ReplacementConflict,
    /// A user recipient was supplied where an operator account is required.
    #[error("replenishment accounts must be operator-owned and distinct from fast recipient")]
    RecipientReuse,
}

/// Durable job containing both economic legs and reconciliation evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplenishmentJob {
    /// Permanent logical job ID.
    pub job_id: B256,
    /// Source operator inventory account.
    pub source_inventory: Address,
    /// Source Zone fallback recipient for withdrawal bounce-back.
    pub source_fallback: Address,
    /// Allowlisted L1 destination-operator treasury.
    pub treasury: Address,
    /// Destination operator pool receiving the deposit.
    pub destination_pool: Address,
    /// Destination deposit refund recipient (the treasury).
    pub deposit_refund: Address,
    /// Gross contributing transfer inventory.
    pub contributions: Vec<InventoryContribution>,
    /// Sum of contributions.
    pub gross_amount: U256,
    /// Current durable stage.
    pub stage: ReplenishmentStage,
    /// Source withdrawal identity, persisted before submission.
    pub withdrawal: Option<WithdrawalRecord>,
    /// Finalized observed treasury credit after withdrawal fees.
    pub treasury_credit: Option<U256>,
    /// Destination deposit identity, persisted before submission.
    pub deposit: Option<DepositRecord>,
    /// Actual final pool credit, never the requested gross amount by assumption.
    pub pool_credit: Option<U256>,
}

impl ReplenishmentJob {
    /// Atomically allocate already released operator inventory.
    pub fn allocate(
        job_id: B256,
        source_inventory: Address,
        source_fallback: Address,
        treasury: Address,
        destination_pool: Address,
        fast_recipient: Address,
        contributions: Vec<InventoryContribution>,
    ) -> Result<Self, ReplenishmentError> {
        if [
            source_inventory,
            source_fallback,
            treasury,
            destination_pool,
        ]
        .contains(&fast_recipient)
        {
            return Err(ReplenishmentError::RecipientReuse);
        }
        if contributions.is_empty() {
            return Err(ReplenishmentError::InvalidContributions);
        }
        let mut ids = HashSet::with_capacity(contributions.len());
        let mut gross_amount = U256::ZERO;
        for contribution in &contributions {
            if contribution.amount.is_zero() || !ids.insert(contribution.transfer_id) {
                return Err(ReplenishmentError::InvalidContributions);
            }
            gross_amount = gross_amount
                .checked_add(contribution.amount)
                .ok_or(ReplenishmentError::InvalidAmount)?;
        }
        Ok(Self {
            job_id,
            source_inventory,
            source_fallback,
            treasury,
            destination_pool,
            deposit_refund: treasury,
            contributions,
            gross_amount,
            stage: ReplenishmentStage::InventoryAllocated,
            withdrawal: None,
            treasury_credit: None,
            deposit: None,
            pool_credit: None,
        })
    }

    /// Persist the exact source withdrawal action before submission.
    pub fn request_withdrawal(
        &mut self,
        record: WithdrawalRecord,
    ) -> Result<bool, ReplenishmentError> {
        match (&self.withdrawal, self.stage) {
            (None, ReplenishmentStage::InventoryAllocated) => {
                self.withdrawal = Some(record);
                self.stage = ReplenishmentStage::WithdrawalRequested;
                Ok(true)
            }
            (Some(existing), ReplenishmentStage::WithdrawalRequested) if existing == &record => {
                Ok(false)
            }
            (Some(existing), _)
                if existing.signer_nonce != record.signer_nonce
                    || existing.transaction_intent_hash != record.transaction_intent_hash =>
            {
                Err(ReplenishmentError::ReplacementConflict)
            }
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Reconcile transaction/batch/queue identities without changing the economic action.
    pub fn reconcile_withdrawal(
        &mut self,
        transaction_hash: Option<B256>,
        accepted_batch_hash: Option<B256>,
        queue_index: Option<u64>,
    ) -> Result<(), ReplenishmentError> {
        let record = self
            .withdrawal
            .as_mut()
            .ok_or(ReplenishmentError::OutOfOrder)?;
        set_once(&mut record.transaction_hash, transaction_hash)?;
        set_once(&mut record.accepted_batch_hash, accepted_batch_hash)?;
        set_once(&mut record.queue_index, queue_index)?;
        Ok(())
    }

    /// Record a consumed withdrawal that bounced. It is not inventory until restored.
    pub fn record_withdrawal_bounce(&mut self) -> Result<bool, ReplenishmentError> {
        match self.stage {
            ReplenishmentStage::WithdrawalRequested => {
                self.stage = ReplenishmentStage::WithdrawalBounced;
                Ok(true)
            }
            ReplenishmentStage::WithdrawalBounced => Ok(false),
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Record finalized treasury funds from the withdrawal leg.
    pub fn record_treasury_credit(&mut self, net_amount: U256) -> Result<bool, ReplenishmentError> {
        if net_amount.is_zero() || net_amount > self.gross_amount {
            return Err(ReplenishmentError::InvalidAmount);
        }
        match (self.stage, self.treasury_credit) {
            (ReplenishmentStage::WithdrawalRequested, None) => {
                self.treasury_credit = Some(net_amount);
                self.stage = ReplenishmentStage::TreasuryFunded;
                Ok(true)
            }
            (ReplenishmentStage::TreasuryFunded, Some(existing)) if existing == net_amount => {
                Ok(false)
            }
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Persist the exact destination deposit action before submission.
    pub fn submit_deposit(&mut self, record: DepositRecord) -> Result<bool, ReplenishmentError> {
        match (&self.deposit, self.stage) {
            (None, ReplenishmentStage::TreasuryFunded) => {
                self.deposit = Some(record);
                self.stage = ReplenishmentStage::DepositSubmitted;
                Ok(true)
            }
            (Some(existing), ReplenishmentStage::DepositSubmitted) if existing == &record => {
                Ok(false)
            }
            (Some(existing), _)
                if existing.signer_nonce != record.signer_nonce
                    || existing.transaction_intent_hash != record.transaction_intent_hash =>
            {
                Err(ReplenishmentError::ReplacementConflict)
            }
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Reconcile the deposit queue/transaction identity after submission ambiguity.
    pub fn reconcile_deposit(
        &mut self,
        transaction_hash: Option<B256>,
        queue_index: Option<u64>,
    ) -> Result<(), ReplenishmentError> {
        let record = self
            .deposit
            .as_mut()
            .ok_or(ReplenishmentError::OutOfOrder)?;
        set_once(&mut record.transaction_hash, transaction_hash)?;
        set_once(&mut record.queue_index, queue_index)?;
        Ok(())
    }

    /// Record a failed destination deposit awaiting operator refund reconciliation.
    pub fn record_deposit_refund_pending(&mut self) -> Result<bool, ReplenishmentError> {
        match self.stage {
            ReplenishmentStage::DepositSubmitted => {
                self.stage = ReplenishmentStage::DepositRefundPending;
                Ok(true)
            }
            ReplenishmentStage::DepositRefundPending => Ok(false),
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Record actual destination pool credit. This is the only availability-increasing event.
    pub fn record_pool_credit(&mut self, net_amount: U256) -> Result<bool, ReplenishmentError> {
        let treasury_credit = self.treasury_credit.ok_or(ReplenishmentError::OutOfOrder)?;
        if net_amount.is_zero() || net_amount > treasury_credit {
            return Err(ReplenishmentError::InvalidAmount);
        }
        match (self.stage, self.pool_credit) {
            (ReplenishmentStage::DepositSubmitted, None) => {
                self.pool_credit = Some(net_amount);
                self.stage = ReplenishmentStage::PoolCredited;
                Ok(true)
            }
            (ReplenishmentStage::PoolCredited, Some(existing)) if existing == net_amount => {
                Ok(false)
            }
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }
}

fn set_once<T: Copy + Eq>(
    slot: &mut Option<T>,
    value: Option<T>,
) -> Result<(), ReplenishmentError> {
    if let Some(value) = value {
        match slot {
            None => *slot = Some(value),
            Some(existing) if *existing == value => {}
            Some(_) => return Err(ReplenishmentError::ReplacementConflict),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_tracks_net_credits_and_idempotent_actions() {
        let mut job = ReplenishmentJob::allocate(
            B256::repeat_byte(1),
            Address::repeat_byte(2),
            Address::repeat_byte(3),
            Address::repeat_byte(4),
            Address::repeat_byte(5),
            Address::repeat_byte(6),
            vec![InventoryContribution {
                transfer_id: B256::repeat_byte(7),
                amount: U256::from(100),
            }],
        )
        .unwrap();
        let withdrawal = WithdrawalRecord {
            transaction_intent_hash: B256::repeat_byte(8),
            signer_nonce: 4,
            fallback_nonce: 9,
            transaction_hash: None,
            accepted_batch_hash: None,
            queue_index: None,
        };
        assert_eq!(job.request_withdrawal(withdrawal.clone()), Ok(true));
        assert_eq!(job.request_withdrawal(withdrawal), Ok(false));
        job.record_treasury_credit(U256::from(98)).unwrap();
        job.submit_deposit(DepositRecord {
            transaction_intent_hash: B256::repeat_byte(9),
            signer_nonce: 5,
            queue_index: None,
            transaction_hash: None,
        })
        .unwrap();
        job.record_pool_credit(U256::from(97)).unwrap();
        assert_eq!(job.pool_credit, Some(U256::from(97)));
        assert_eq!(job.record_pool_credit(U256::from(97)), Ok(false));
    }

    #[test]
    fn recipient_cannot_be_reused_for_background_job() {
        let recipient = Address::repeat_byte(2);
        assert_eq!(
            ReplenishmentJob::allocate(
                B256::repeat_byte(1),
                recipient,
                Address::repeat_byte(3),
                Address::repeat_byte(4),
                Address::repeat_byte(5),
                recipient,
                vec![InventoryContribution {
                    transfer_id: B256::repeat_byte(7),
                    amount: U256::from(1)
                }],
            ),
            Err(ReplenishmentError::RecipientReuse)
        );
    }
}
