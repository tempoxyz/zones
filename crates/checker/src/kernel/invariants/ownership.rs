//! Ownership and origin validation.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::Address;

use super::{
    Cursor, InvariantCode, InvariantViolation, PortalIdentity, State, ZoneState, violation,
};
use crate::kernel::state::{
    DepositOwner, FallbackState, StateKey, StateValue, TokenPhase, WithdrawalOrigin,
    WithdrawalOwner,
};

/// Validate fallback links, object ownership, and one-to-one origin relationships.
pub(super) fn validate(
    state: &State,
    identity: PortalIdentity,
    zone: &ZoneState,
    portal_cursor: Cursor,
) -> Result<(), InvariantViolation> {
    OwnershipValidator::new(state, identity, zone, portal_cursor).validate()
}

/// Per-row ownership checks and the origin sets they update.
struct OwnershipValidator<'a> {
    state: &'a State,
    identity: PortalIdentity,
    zone: &'a ZoneState,
    portal_cursor: Cursor,
    deposit_origins: BTreeSet<crate::kernel::state::DepositId>,
    withdrawal_origins: BTreeSet<crate::kernel::state::WithdrawalId>,
    refund_totals: BTreeMap<(bool, Address, Address), u128>,
}

impl<'a> OwnershipValidator<'a> {
    /// Initialize validation with empty origin and refund accumulators.
    fn new(
        state: &'a State,
        identity: PortalIdentity,
        zone: &'a ZoneState,
        portal_cursor: Cursor,
    ) -> Self {
        Self {
            state,
            identity,
            zone,
            portal_cursor,
            deposit_origins: BTreeSet::new(),
            withdrawal_origins: BTreeSet::new(),
            refund_totals: BTreeMap::new(),
        }
    }

    /// Validate every persisted row against its ownership relationship.
    fn validate(mut self) -> Result<(), InvariantViolation> {
        let state = self.state;
        for (key, value) in state.rows() {
            self.validate_row(*key, value)?;
        }
        Ok(())
    }

    /// Dispatch one state row to its ownership rule.
    fn validate_row(
        &mut self,
        key: StateKey,
        value: &StateValue,
    ) -> Result<(), InvariantViolation> {
        match (key, value) {
            (
                StateKey::Withdrawal(id),
                StateValue::Withdrawal(WithdrawalOwner::PendingUser { data, fallback }),
            )
            | (
                StateKey::Withdrawal(id),
                StateValue::Withdrawal(WithdrawalOwner::Finalized {
                    data,
                    origin: WithdrawalOrigin::User { fallback },
                }),
            ) => self.require_held_fallback(id, data, *fallback, key),
            (StateKey::Deposit(id), StateValue::Deposit(DepositOwner::BounceBack { .. })) => {
                self.validate_bounce_back_deposit(key, id, value)
            }
            (StateKey::Fallback(fallback), StateValue::Fallback(FallbackState::Held { .. })) => {
                self.validate_held_fallback(key, fallback, value)
            }
            (StateKey::Fallback(fallback), StateValue::Fallback(FallbackState::Queued { .. })) => {
                self.validate_queued_fallback(key, fallback, value)
            }
            (StateKey::PortalRefund(id), _) if id.deposit.portal != self.identity.portal => {
                Err(violation(InvariantCode::OwnerLink, Some(key)))
            }
            (StateKey::InboxRefund(id), _) if id.withdrawal.zone_id != self.identity.zone_id => {
                Err(violation(InvariantCode::OwnerLink, Some(key)))
            }
            (StateKey::Deposit(id), StateValue::Deposit(owner)) => {
                self.validate_deposit(key, id, owner)
            }
            (StateKey::Withdrawal(id), StateValue::Withdrawal(owner)) => {
                self.validate_withdrawal(key, id, owner)
            }
            (StateKey::PortalRefund(id), StateValue::PortalRefund(credit)) => {
                self.validate_portal_refund(key, id, credit)
            }
            (StateKey::InboxRefund(id), StateValue::InboxRefund(credit)) => {
                self.validate_inbox_refund(key, id, credit)
            }
            _ => Ok(()),
        }
    }

    /// Require a bounce-back deposit to link to its queued fallback.
    fn validate_bounce_back_deposit(
        &self,
        key: StateKey,
        id: crate::kernel::state::DepositId,
        value: &StateValue,
    ) -> Result<(), InvariantViolation> {
        let StateValue::Deposit(DepositOwner::BounceBack {
            withdrawal,
            token,
            fallback_nonce,
            amount,
        }) = value
        else {
            return Err(violation(InvariantCode::OwnerLink, Some(key)));
        };
        let fallback =
            crate::kernel::state::FallbackId::new(withdrawal.zone_id, fallback_nonce.get())
                .ok_or_else(|| violation(InvariantCode::OwnerLink, Some(key)))?;
        let linked = matches!(
            self.state.rows().get(&StateKey::Fallback(fallback)),
            Some(StateValue::Fallback(FallbackState::Queued {
                withdrawal: actual_withdrawal,
                token: actual_token,
                amount: actual_amount,
                deposit: actual_deposit,
            })) if actual_withdrawal == withdrawal
                && actual_token == token
                && actual_amount == amount
                && actual_deposit == &id
        );
        if linked {
            Ok(())
        } else {
            Err(violation(InvariantCode::OwnerLink, Some(key)))
        }
    }

    /// Require a held fallback to link to its user withdrawal.
    fn validate_held_fallback(
        &self,
        key: StateKey,
        fallback: crate::kernel::state::FallbackId,
        value: &StateValue,
    ) -> Result<(), InvariantViolation> {
        let StateValue::Fallback(FallbackState::Held {
            withdrawal,
            token,
            amount,
        }) = value
        else {
            unreachable!("called only for held fallbacks")
        };
        let linked = matches!(
            self.state.rows().get(&StateKey::Withdrawal(*withdrawal)),
            Some(StateValue::Withdrawal(WithdrawalOwner::PendingUser { data, fallback: actual }))
                | Some(StateValue::Withdrawal(WithdrawalOwner::Finalized { data, origin: WithdrawalOrigin::User { fallback: actual } }))
                if actual == &fallback && data.token == *token && data.amount == *amount
        );
        if linked {
            Ok(())
        } else {
            Err(violation(InvariantCode::OwnerLink, Some(key)))
        }
    }

    /// Require a queued fallback to link to its bounce-back deposit.
    fn validate_queued_fallback(
        &self,
        key: StateKey,
        fallback: crate::kernel::state::FallbackId,
        value: &StateValue,
    ) -> Result<(), InvariantViolation> {
        let StateValue::Fallback(FallbackState::Queued {
            withdrawal,
            token,
            amount,
            deposit,
        }) = value
        else {
            unreachable!("called only for queued fallbacks")
        };
        let linked = matches!(
            self.state.rows().get(&StateKey::Deposit(*deposit)),
            Some(StateValue::Deposit(DepositOwner::BounceBack { withdrawal: actual_withdrawal, token: actual_token, fallback_nonce, amount: actual_amount }))
                if actual_withdrawal == withdrawal && actual_token == token && *fallback_nonce == fallback.nonce && actual_amount == amount
        );
        if linked {
            Ok(())
        } else {
            Err(violation(InvariantCode::OwnerLink, Some(key)))
        }
    }

    /// Validate one pending deposit and record its ordinary origin.
    fn validate_deposit(
        &mut self,
        key: StateKey,
        id: crate::kernel::state::DepositId,
        owner: &DepositOwner,
    ) -> Result<(), InvariantViolation> {
        let invalid = id.portal != self.identity.portal
            || id.number.get() <= self.zone.processed_deposit.number
            || id.number.get() > self.portal_cursor.number
            || match owner {
                DepositOwner::Ordinary(deposit) => deposit.tempo_refund_recipient.is_zero(),
                DepositOwner::BounceBack {
                    withdrawal, token, ..
                } => {
                    withdrawal.zone_id != self.identity.zone_id
                        || !self.is_zone_enabled_token(*token)
                }
            };
        if invalid {
            return Err(violation(InvariantCode::Bounds, Some(key)));
        }
        if matches!(owner, DepositOwner::Ordinary(_)) && !self.deposit_origins.insert(id) {
            return Err(violation(InvariantCode::OriginExclusivity, Some(key)));
        }
        Ok(())
    }

    /// Validate one withdrawal and record its originating withdrawal or deposit.
    fn validate_withdrawal(
        &mut self,
        key: StateKey,
        id: crate::kernel::state::WithdrawalId,
        owner: &WithdrawalOwner,
    ) -> Result<(), InvariantViolation> {
        let token = match owner {
            WithdrawalOwner::PendingFailedDeposit {
                deposit,
                token,
                recipient,
                ..
            } => {
                if recipient.is_zero()
                    || deposit.portal != self.identity.portal
                    || deposit.number.get() > self.zone.processed_deposit.number
                    || !self.deposit_origins.insert(*deposit)
                {
                    return Err(violation(InvariantCode::OriginExclusivity, Some(key)));
                }
                *token
            }
            WithdrawalOwner::PendingUser { data, .. } => data.token,
            WithdrawalOwner::Finalized { data, origin } => {
                if let WithdrawalOrigin::FailedDeposit { deposit } = origin
                    && (data.to.is_zero()
                        || deposit.portal != self.identity.portal
                        || deposit.number.get() > self.zone.processed_deposit.number
                        || !self.deposit_origins.insert(*deposit))
                {
                    return Err(violation(InvariantCode::OriginExclusivity, Some(key)));
                }
                data.token
            }
        };
        if id.zone_id != self.identity.zone_id
            || id.index >= self.zone.next_withdrawal_index
            || !self.is_zone_enabled_token(token)
        {
            return Err(violation(InvariantCode::Identity, Some(key)));
        }
        if !self.withdrawal_origins.insert(id) {
            return Err(violation(InvariantCode::OriginExclusivity, Some(key)));
        }
        Ok(())
    }

    /// Validate and total a Portal-side refund credit.
    fn validate_portal_refund(
        &mut self,
        key: StateKey,
        id: crate::kernel::state::PortalRefundId,
        credit: &crate::kernel::state::RefundCredit,
    ) -> Result<(), InvariantViolation> {
        let invalid = id.deposit.portal != self.identity.portal
            || id.recipient.is_zero()
            || !self.is_zone_enabled_token(id.token)
            || id.deposit.number.get() > self.zone.processed_deposit.number
            || !self.deposit_origins.insert(id.deposit)
            || !self.add_refund((false, id.token, id.recipient), credit.amount);
        if invalid {
            Err(violation(InvariantCode::Refund, Some(key)))
        } else {
            Ok(())
        }
    }

    /// Validate and total an inbox-side refund credit.
    fn validate_inbox_refund(
        &mut self,
        key: StateKey,
        id: crate::kernel::state::InboxRefundId,
        credit: &crate::kernel::state::RefundCredit,
    ) -> Result<(), InvariantViolation> {
        let invalid = id.withdrawal.zone_id != self.identity.zone_id
            || id.recipient.is_zero()
            || !self.is_zone_enabled_token(id.token)
            || id.withdrawal.index >= self.zone.next_withdrawal_index
            || !self.withdrawal_origins.insert(id.withdrawal)
            || !self.add_refund((true, id.token, id.recipient), credit.amount);
        if invalid {
            Err(violation(InvariantCode::Refund, Some(key)))
        } else {
            Ok(())
        }
    }
    /// Require a user withdrawal to retain the fallback record that backs it.
    fn require_held_fallback(
        &self,
        withdrawal: crate::kernel::state::WithdrawalId,
        data: &crate::kernel::state::Withdrawal,
        fallback: crate::kernel::state::FallbackId,
        location: StateKey,
    ) -> Result<(), InvariantViolation> {
        if matches!(
            self.state.rows().get(&StateKey::Fallback(fallback)),
            Some(StateValue::Fallback(FallbackState::Held {
                withdrawal: actual_withdrawal,
                token,
                amount,
            })) if *actual_withdrawal == withdrawal && *token == data.token && *amount == data.amount
        ) {
            Ok(())
        } else {
            Err(violation(InvariantCode::OwnerLink, Some(location)))
        }
    }

    /// Return whether a token is enabled in the created Zone state.
    fn is_zone_enabled_token(&self, token: Address) -> bool {
        matches!(self.state.rows().get(&StateKey::Token(token)), Some(StateValue::Token(t)) if t.phase == TokenPhase::ZoneEnabled)
    }

    /// Add one refund credit to its direction, token, and recipient total.
    fn add_refund(&mut self, account: (bool, Address, Address), amount: u128) -> bool {
        self.refund_totals
            .get(&account)
            .copied()
            .unwrap_or_default()
            .checked_add(amount)
            .map(|total| self.refund_totals.insert(account, total))
            .is_some()
    }
}
