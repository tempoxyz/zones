//! Idempotent source and destination transfer progression.

use alloy_primitives::{Address, B256, U256};
use zone_primitives::fast_transfer::{RejectionReason, TransferIntent};

/// State-machine invariant failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StateError {
    /// Same ID was reused with a different complete intent hash.
    #[error("transfer ID already binds another intent")]
    IntentConflict,
    /// A terminal destination decision cannot be replaced.
    #[error("conflicting terminal destination outcome")]
    TerminalConflict,
    /// Source decision does not match its committed destination decision.
    #[error("source disposition does not match destination decision")]
    DispositionConflict,
    /// Transition requires a prior durable decision.
    #[error("transfer transition is out of order")]
    OutOfOrder,
    /// Principal plus fee overflowed.
    #[error("escrow amount overflow")]
    AmountOverflow,
}

/// Immutable destination terminal decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestinationDecision {
    /// Existing pool tokens were atomically debited and credited to the recipient.
    Paid {
        /// Pool account.
        pool: Address,
        /// Ordinary token recipient.
        recipient: Address,
        /// Exact principal.
        principal: U256,
    },
    /// Permanent no-payment record.
    Rejected(RejectionReason),
}

/// Destination-side durable record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationRecord {
    /// Stable transfer ID.
    pub transfer_id: B256,
    /// Full original intent hash.
    pub intent_hash: B256,
    /// `None` means valid lock evidence is durable but execution has no terminal result.
    pub decision: Option<DestinationDecision>,
}

impl DestinationRecord {
    /// Bind a newly received valid lock to its exact intent.
    pub fn new(intent: &TransferIntent) -> Self {
        Self {
            transfer_id: intent.transfer_id(),
            intent_hash: intent.intent_hash(),
            decision: None,
        }
    }

    /// Check idempotent delivery before executing token effects.
    pub fn check_intent(&self, intent: &TransferIntent) -> Result<(), StateError> {
        if self.transfer_id == intent.transfer_id() && self.intent_hash == intent.intent_hash() {
            Ok(())
        } else {
            Err(StateError::IntentConflict)
        }
    }

    /// Commit the result chosen in the same execution journal as pool token movement.
    pub fn decide(&mut self, decision: DestinationDecision) -> Result<bool, StateError> {
        match self.decision {
            None => {
                self.decision = Some(decision);
                Ok(true)
            }
            Some(existing) if existing == decision => Ok(false),
            Some(_) => Err(StateError::TerminalConflict),
        }
    }
}

/// Source escrow progression; policy-blocked disposition remains an explicit liability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceStage {
    /// Principal plus fee is held in native escrow.
    Locked,
    /// Paid decision is durable; release to reimbursement account is pending.
    PaidAwaitingRelease,
    /// Rejected decision is durable; refund to sender is pending.
    RejectedAwaitingRefund,
    /// Escrow was transferred exactly once to the reimbursement account.
    Released,
    /// Escrow was returned exactly once to the sender.
    Refunded,
}

/// Source-side durable record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRecord {
    /// Stable transfer ID.
    pub transfer_id: B256,
    /// Exact intent body hash.
    pub intent_hash: B256,
    /// Escrowed principal plus exact fee.
    pub escrow_amount: U256,
    /// Current monotonic source stage.
    pub stage: SourceStage,
}

impl SourceRecord {
    /// Create a committed lock record.
    pub fn locked(intent: &TransferIntent) -> Result<Self, StateError> {
        Ok(Self {
            transfer_id: intent.transfer_id(),
            intent_hash: intent.intent_hash(),
            escrow_amount: intent
                .principal
                .checked_add(intent.fee)
                .ok_or(StateError::AmountOverflow)?,
            stage: SourceStage::Locked,
        })
    }

    /// Durably remember the destination decision independently of token disposition.
    pub fn record_decision(&mut self, decision: DestinationDecision) -> Result<bool, StateError> {
        let next = match decision {
            DestinationDecision::Paid { .. } => SourceStage::PaidAwaitingRelease,
            DestinationDecision::Rejected(_) => SourceStage::RejectedAwaitingRefund,
        };
        match self.stage {
            SourceStage::Locked => {
                self.stage = next;
                Ok(true)
            }
            existing if existing == next => Ok(false),
            SourceStage::Released if next == SourceStage::PaidAwaitingRelease => Ok(false),
            SourceStage::Refunded if next == SourceStage::RejectedAwaitingRefund => Ok(false),
            _ => Err(StateError::DispositionConflict),
        }
    }

    /// Mark successful policy-checked escrow movement. Token movement and this update must
    /// be one native execution transaction.
    pub fn complete_disposition(&mut self) -> Result<bool, StateError> {
        match self.stage {
            SourceStage::PaidAwaitingRelease => {
                self.stage = SourceStage::Released;
                Ok(true)
            }
            SourceStage::RejectedAwaitingRefund => {
                self.stage = SourceStage::Refunded;
                Ok(true)
            }
            SourceStage::Released | SourceStage::Refunded => Ok(false),
            SourceStage::Locked => Err(StateError::OutOfOrder),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use zone_primitives::fast_transfer::{AssetId, TransferIntent, ZoneDomain};

    use super::*;

    fn intent() -> TransferIntent {
        let source = ZoneDomain {
            l1_chain_id: 1,
            zone_id: 1,
            chain_id: 101,
            portal: Address::repeat_byte(1),
            authority_epoch: 1,
            roster_hash: B256::repeat_byte(1),
            protocol_version: 1,
        };
        let destination = ZoneDomain {
            zone_id: 2,
            chain_id: 102,
            portal: Address::repeat_byte(2),
            roster_hash: B256::repeat_byte(2),
            ..source
        };
        TransferIntent {
            source,
            destination,
            asset: AssetId {
                l1_token: Address::repeat_byte(3),
                source_token: Address::repeat_byte(4),
                destination_token: Address::repeat_byte(5),
                decimals: 6,
            },
            sender: Address::repeat_byte(6),
            recipient: Address::repeat_byte(7),
            refund_account: Address::repeat_byte(6),
            destination_pool: Address::repeat_byte(8),
            reimbursement_account: Address::repeat_byte(9),
            principal: U256::from(10),
            fee: U256::from(1),
            quote_id: B256::repeat_byte(10),
            destination_expiry_height: 100,
            transfer_nonce: 1,
        }
    }

    #[test]
    fn terminal_choice_and_source_disposition_are_immutable() {
        let intent = intent();
        let paid = DestinationDecision::Paid {
            pool: intent.destination_pool,
            recipient: intent.recipient,
            principal: intent.principal,
        };
        let mut destination = DestinationRecord::new(&intent);
        assert_eq!(destination.decide(paid), Ok(true));
        assert_eq!(destination.decide(paid), Ok(false));
        assert_eq!(
            destination.decide(DestinationDecision::Rejected(RejectionReason::Cancelled)),
            Err(StateError::TerminalConflict)
        );

        let mut source = SourceRecord::locked(&intent).unwrap();
        source.record_decision(paid).unwrap();
        assert_eq!(source.complete_disposition(), Ok(true));
        assert_eq!(source.complete_disposition(), Ok(false));
        assert!(
            source
                .record_decision(DestinationDecision::Rejected(RejectionReason::Cancelled))
                .is_err()
        );
    }
}
