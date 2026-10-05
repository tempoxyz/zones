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

/// Maximum candidate transaction hashes retained for one immutable same-nonce action.
pub const MAX_SUBMISSION_HASHES: usize = 16;

/// An amount in an explicitly named token. Zero is a valid observed fee.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenAmount {
    /// Token in which the amount is denominated.
    pub token: Address,
    /// Exact observed amount.
    pub amount: U256,
}

/// Actual cost of one canonical transaction receipt, separate from configured fee caps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionCost {
    /// Canonical transaction whose receipt supplied the cost.
    pub transaction_hash: B256,
    /// Effective fee payer authenticated from the Tempo transaction.
    pub payer: Address,
    /// Fee token. `None` is the protocol's valid zero-fee representation.
    pub token: Option<Address>,
    /// Exact `gasUsed * effectiveGasPrice` from the canonical receipt.
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
    /// Bounced inventory was observed back in the source operator account.
    InventoryRestored,
    /// L1 treasury has a finalized observed credit.
    TreasuryFunded,
    /// Destination encrypted deposit was submitted or is being reconciled.
    DepositSubmitted,
    /// Deposit failed; wait for actual treasury refund or claimable refund.
    DepositRefundPending,
    /// The failed deposit's funds were observed back in the L1 treasury.
    TreasuryRefunded,
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
    /// Actual numeric Outbox fallback identity, assigned only by canonical inclusion.
    pub fallback_nonce: Option<u64>,
    /// Actual sequential Outbox withdrawal identity, assigned by the same atomic receipt.
    pub withdrawal_index: Option<u64>,
    /// Public sender commitment derived from sender, canonical source tx hash, and fallback nonce.
    pub sender_tag: Option<B256>,
    /// Bounded append-only candidate/replacement hashes for this same-nonce action.
    pub submission_hashes: Vec<B256>,
    /// Canonical source action transaction hash, which may differ from every earlier candidate.
    pub transaction_hash: Option<B256>,
    /// Accepted source batch hash, if finalized.
    pub accepted_batch_hash: Option<B256>,
    /// Sequential withdrawal queue identity, if assigned.
    pub queue_index: Option<u64>,
    /// Actual source transaction receipt cost, denominated in its independent fee asset.
    pub source_transaction_cost: Option<TransactionCost>,
    /// Outbox withdrawal fee burned separately from replenishment principal.
    pub withdrawal_fee: Option<TokenAmount>,
    /// Actual canonical Tempo withdrawal-processing transaction cost. This is evidence, not a
    /// deduction from this job's principal because a processing transaction can contain jobs.
    pub l1_transaction_cost: Option<TransactionCost>,
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
    /// Bounded append-only candidate/replacement hashes for this same-nonce action.
    pub submission_hashes: Vec<B256>,
    /// Canonical L1 deposit transaction hash, which may differ from earlier candidates.
    pub transaction_hash: Option<B256>,
    /// Actual canonical L1 deposit transaction receipt cost.
    pub transaction_cost: Option<TransactionCost>,
    /// Portal deposit fee charged separately from the destination net pool credit.
    pub deposit_fee: Option<TokenAmount>,
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
    /// More distinct replacement hashes were observed than the durable evidence bound permits.
    #[error("submission hash history exceeds its configured bound")]
    SubmissionHistoryFull,
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
    /// Actual source inventory restored after a withdrawal bounce.
    pub restored_inventory: Option<U256>,
    /// Destination deposit identity, persisted before submission.
    pub deposit: Option<DepositRecord>,
    /// Actual final pool credit, never the requested gross amount by assumption.
    pub pool_credit: Option<U256>,
    /// Actual L1 treasury refund observed after a destination deposit failure.
    pub treasury_refund: Option<U256>,
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
            restored_inventory: None,
            deposit: None,
            pool_credit: None,
            treasury_refund: None,
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

    /// Append a source submission candidate without selecting it as canonical evidence.
    pub fn record_withdrawal_submission(
        &mut self,
        transaction_hash: B256,
    ) -> Result<bool, ReplenishmentError> {
        let record = self
            .withdrawal
            .as_mut()
            .ok_or(ReplenishmentError::OutOfOrder)?;
        append_submission_hash(&mut record.submission_hashes, transaction_hash)
    }

    /// Bind identities and accounting emitted by the one canonical atomic allocation receipt.
    #[allow(clippy::too_many_arguments)]
    pub fn bind_canonical_withdrawal(
        &mut self,
        transaction_hash: B256,
        fallback_nonce: u64,
        withdrawal_index: u64,
        sender_tag: B256,
        source_transaction_cost: TransactionCost,
        withdrawal_fee: TokenAmount,
    ) -> Result<(), ReplenishmentError> {
        if fallback_nonce == 0
            || transaction_hash.is_zero()
            || sender_tag.is_zero()
            || source_transaction_cost.transaction_hash != transaction_hash
        {
            return Err(ReplenishmentError::ReplacementConflict);
        }
        let record = self
            .withdrawal
            .as_mut()
            .ok_or(ReplenishmentError::OutOfOrder)?;
        if !can_set_once(record.transaction_hash, transaction_hash)
            || !can_set_once(record.fallback_nonce, fallback_nonce)
            || !can_set_once(record.withdrawal_index, withdrawal_index)
            || !can_set_once(record.sender_tag, sender_tag)
            || !can_set_once(record.source_transaction_cost, source_transaction_cost)
            || !can_set_once(record.withdrawal_fee, withdrawal_fee)
        {
            return Err(ReplenishmentError::ReplacementConflict);
        }
        append_submission_hash(&mut record.submission_hashes, transaction_hash)?;
        set_once(&mut record.transaction_hash, Some(transaction_hash))?;
        set_once(&mut record.fallback_nonce, Some(fallback_nonce))?;
        set_once(&mut record.withdrawal_index, Some(withdrawal_index))?;
        set_once(&mut record.sender_tag, Some(sender_tag))?;
        set_once(
            &mut record.source_transaction_cost,
            Some(source_transaction_cost),
        )?;
        set_once(&mut record.withdrawal_fee, Some(withdrawal_fee))?;
        Ok(())
    }

    /// Bind the actual receipt cost of the Tempo withdrawal-processing transaction.
    pub fn record_l1_withdrawal_cost(
        &mut self,
        cost: TransactionCost,
    ) -> Result<(), ReplenishmentError> {
        let record = self
            .withdrawal
            .as_mut()
            .ok_or(ReplenishmentError::OutOfOrder)?;
        set_once(&mut record.l1_transaction_cost, Some(cost))
    }

    /// Record a consumed withdrawal that bounced. It is not inventory until restored.
    pub fn record_withdrawal_bounce(&mut self) -> Result<bool, ReplenishmentError> {
        if !self.withdrawal_evidence_complete() {
            return Err(ReplenishmentError::OutOfOrder);
        }
        match self.stage {
            ReplenishmentStage::WithdrawalRequested => {
                self.stage = ReplenishmentStage::WithdrawalBounced;
                Ok(true)
            }
            ReplenishmentStage::WithdrawalBounced => Ok(false),
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Record actual restoration of bounced inventory in the source Zone.
    pub fn record_inventory_restored(&mut self, amount: U256) -> Result<bool, ReplenishmentError> {
        if amount != self.gross_amount {
            return Err(ReplenishmentError::InvalidAmount);
        }
        match (self.stage, self.restored_inventory) {
            (ReplenishmentStage::WithdrawalBounced, None) => {
                self.restored_inventory = Some(amount);
                self.stage = ReplenishmentStage::InventoryRestored;
                Ok(true)
            }
            (ReplenishmentStage::InventoryRestored, Some(existing)) if existing == amount => {
                Ok(false)
            }
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Record finalized treasury funds from the withdrawal leg.
    pub fn record_treasury_credit(&mut self, net_amount: U256) -> Result<bool, ReplenishmentError> {
        if net_amount != self.gross_amount || !self.withdrawal_evidence_complete() {
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

    /// Append an L1 deposit submission candidate without treating RPC acceptance as success.
    pub fn record_deposit_submission(
        &mut self,
        transaction_hash: B256,
    ) -> Result<bool, ReplenishmentError> {
        let record = self
            .deposit
            .as_mut()
            .ok_or(ReplenishmentError::OutOfOrder)?;
        append_submission_hash(&mut record.submission_hashes, transaction_hash)
    }

    /// Bind the canonical deposit receipt and its explicit gas/deposit-fee buckets.
    pub fn bind_canonical_deposit(
        &mut self,
        transaction_hash: B256,
        transaction_cost: TransactionCost,
        deposit_fee: TokenAmount,
    ) -> Result<(), ReplenishmentError> {
        if transaction_hash.is_zero() || transaction_cost.transaction_hash != transaction_hash {
            return Err(ReplenishmentError::ReplacementConflict);
        }
        let record = self
            .deposit
            .as_mut()
            .ok_or(ReplenishmentError::OutOfOrder)?;
        if !can_set_once(record.transaction_hash, transaction_hash)
            || !can_set_once(record.transaction_cost, transaction_cost)
            || !can_set_once(record.deposit_fee, deposit_fee)
        {
            return Err(ReplenishmentError::ReplacementConflict);
        }
        append_submission_hash(&mut record.submission_hashes, transaction_hash)?;
        set_once(&mut record.transaction_hash, Some(transaction_hash))?;
        set_once(&mut record.transaction_cost, Some(transaction_cost))?;
        set_once(&mut record.deposit_fee, Some(deposit_fee))?;
        Ok(())
    }

    /// Record a failed destination deposit awaiting operator refund reconciliation.
    pub fn record_deposit_refund_pending(&mut self) -> Result<bool, ReplenishmentError> {
        if !self.deposit_evidence_complete() {
            return Err(ReplenishmentError::OutOfOrder);
        }
        match self.stage {
            ReplenishmentStage::DepositSubmitted => {
                self.stage = ReplenishmentStage::DepositRefundPending;
                Ok(true)
            }
            ReplenishmentStage::DepositRefundPending => Ok(false),
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Record actual treasury funds restored after a failed destination deposit.
    pub fn record_treasury_refund(&mut self, amount: U256) -> Result<bool, ReplenishmentError> {
        let treasury_credit = self.treasury_credit.ok_or(ReplenishmentError::OutOfOrder)?;
        if amount.is_zero() || amount > treasury_credit {
            return Err(ReplenishmentError::InvalidAmount);
        }
        match (self.stage, self.treasury_refund) {
            (ReplenishmentStage::DepositRefundPending, None) => {
                self.treasury_refund = Some(amount);
                self.stage = ReplenishmentStage::TreasuryRefunded;
                Ok(true)
            }
            (ReplenishmentStage::TreasuryRefunded, Some(existing)) if existing == amount => {
                Ok(false)
            }
            _ => Err(ReplenishmentError::OutOfOrder),
        }
    }

    /// Observed source withdrawal fees, once the treasury leg succeeded.
    pub fn withdrawal_fee(&self) -> Option<U256> {
        self.withdrawal
            .as_ref()
            .and_then(|record| record.withdrawal_fee)
            .map(|fee| fee.amount)
    }

    /// Observed destination deposit fees, once the pool was actually credited.
    pub fn deposit_fee(&self) -> Option<U256> {
        self.deposit
            .as_ref()
            .and_then(|record| record.deposit_fee)
            .map(|fee| fee.amount)
    }

    /// Record actual destination pool credit. This is the only availability-increasing event.
    pub fn record_pool_credit(&mut self, net_amount: U256) -> Result<bool, ReplenishmentError> {
        let treasury_credit = self.treasury_credit.ok_or(ReplenishmentError::OutOfOrder)?;
        let deposit_fee = self
            .deposit
            .as_ref()
            .and_then(|record| record.deposit_fee)
            .ok_or(ReplenishmentError::OutOfOrder)?;
        if net_amount.is_zero()
            || net_amount.checked_add(deposit_fee.amount) != Some(treasury_credit)
            || !self.deposit_evidence_complete()
        {
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

    fn withdrawal_evidence_complete(&self) -> bool {
        self.withdrawal.as_ref().is_some_and(|record| {
            record.fallback_nonce.is_some()
                && record.withdrawal_index.is_some()
                && record.sender_tag.is_some()
                && record.transaction_hash.is_some()
                && record.accepted_batch_hash.is_some()
                && record.queue_index.is_some()
                && record.source_transaction_cost.is_some()
                && record.withdrawal_fee.is_some()
                && record.l1_transaction_cost.is_some()
        })
    }

    fn deposit_evidence_complete(&self) -> bool {
        self.deposit.as_ref().is_some_and(|record| {
            record.queue_index.is_some()
                && record.transaction_hash.is_some()
                && record.transaction_cost.is_some()
                && record.deposit_fee.is_some()
        })
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

fn can_set_once<T: Copy + Eq>(slot: Option<T>, value: T) -> bool {
    slot.is_none_or(|existing| existing == value)
}

fn append_submission_hash(
    history: &mut Vec<B256>,
    transaction_hash: B256,
) -> Result<bool, ReplenishmentError> {
    if transaction_hash.is_zero() {
        return Err(ReplenishmentError::ReplacementConflict);
    }
    if history.contains(&transaction_hash) {
        return Ok(false);
    }
    if history.len() >= MAX_SUBMISSION_HASHES {
        return Err(ReplenishmentError::SubmissionHistoryFull);
    }
    history.push(transaction_hash);
    Ok(true)
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
            fallback_nonce: None,
            withdrawal_index: None,
            sender_tag: None,
            submission_hashes: Vec::new(),
            transaction_hash: None,
            accepted_batch_hash: None,
            queue_index: None,
            source_transaction_cost: None,
            withdrawal_fee: None,
            l1_transaction_cost: None,
        };
        assert_eq!(job.request_withdrawal(withdrawal.clone()), Ok(true));
        assert_eq!(job.request_withdrawal(withdrawal), Ok(false));
        let source_hash = B256::repeat_byte(10);
        job.bind_canonical_withdrawal(
            source_hash,
            9,
            11,
            B256::repeat_byte(12),
            TransactionCost {
                transaction_hash: source_hash,
                payer: Address::repeat_byte(2),
                token: None,
                amount: U256::ZERO,
            },
            TokenAmount {
                token: Address::repeat_byte(13),
                amount: U256::ZERO,
            },
        )
        .unwrap();
        job.reconcile_withdrawal(None, Some(B256::repeat_byte(14)), Some(3))
            .unwrap();
        job.record_l1_withdrawal_cost(TransactionCost {
            transaction_hash: B256::repeat_byte(15),
            payer: Address::repeat_byte(16),
            token: None,
            amount: U256::ZERO,
        })
        .unwrap();
        job.record_treasury_credit(U256::from(100)).unwrap();
        job.submit_deposit(DepositRecord {
            transaction_intent_hash: B256::repeat_byte(9),
            signer_nonce: 5,
            queue_index: None,
            submission_hashes: Vec::new(),
            transaction_hash: None,
            transaction_cost: None,
            deposit_fee: None,
        })
        .unwrap();
        let deposit_hash = B256::repeat_byte(17);
        job.bind_canonical_deposit(
            deposit_hash,
            TransactionCost {
                transaction_hash: deposit_hash,
                payer: Address::repeat_byte(4),
                token: None,
                amount: U256::ZERO,
            },
            TokenAmount {
                token: Address::repeat_byte(13),
                amount: U256::from(3),
            },
        )
        .unwrap();
        job.reconcile_deposit(None, Some(4)).unwrap();
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

    #[test]
    fn same_nonce_replacement_keeps_history_and_binds_actual_identity_once() {
        let mut job = ReplenishmentJob::allocate(
            B256::repeat_byte(31),
            Address::repeat_byte(2),
            Address::repeat_byte(3),
            Address::repeat_byte(4),
            Address::repeat_byte(5),
            Address::repeat_byte(6),
            vec![InventoryContribution {
                transfer_id: B256::repeat_byte(7),
                amount: U256::from(10),
            }],
        )
        .unwrap();
        job.request_withdrawal(WithdrawalRecord {
            transaction_intent_hash: B256::repeat_byte(8),
            signer_nonce: 44,
            fallback_nonce: None,
            withdrawal_index: None,
            sender_tag: None,
            submission_hashes: Vec::new(),
            transaction_hash: None,
            accepted_batch_hash: None,
            queue_index: None,
            source_transaction_cost: None,
            withdrawal_fee: None,
            l1_transaction_cost: None,
        })
        .unwrap();
        let first = B256::repeat_byte(9);
        let replacement = B256::repeat_byte(10);
        assert_eq!(job.record_withdrawal_submission(first), Ok(true));
        assert_eq!(job.record_withdrawal_submission(first), Ok(false));
        job.bind_canonical_withdrawal(
            replacement,
            12,
            13,
            B256::repeat_byte(14),
            TransactionCost {
                transaction_hash: replacement,
                payer: Address::repeat_byte(15),
                token: None,
                amount: U256::ZERO,
            },
            TokenAmount {
                token: Address::repeat_byte(16),
                amount: U256::ZERO,
            },
        )
        .unwrap();
        let record = job.withdrawal.as_ref().unwrap();
        assert_eq!(record.signer_nonce, 44);
        assert_eq!(record.fallback_nonce, Some(12));
        assert_eq!(record.submission_hashes, [first, replacement]);
        assert_eq!(record.transaction_hash, Some(replacement));
        assert_eq!(
            job.bind_canonical_withdrawal(
                B256::repeat_byte(11),
                15,
                13,
                B256::repeat_byte(14),
                TransactionCost {
                    transaction_hash: B256::repeat_byte(11),
                    payer: Address::repeat_byte(15),
                    token: None,
                    amount: U256::ZERO,
                },
                TokenAmount {
                    token: Address::repeat_byte(16),
                    amount: U256::ZERO,
                },
            ),
            Err(ReplenishmentError::ReplacementConflict)
        );
        assert_eq!(
            job.withdrawal.as_ref().unwrap().submission_hashes,
            [first, replacement]
        );
    }
}
