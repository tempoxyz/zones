//! Tempo transition application.

use super::*;

/// Apply authenticated Tempo operations before their corresponding Zone facts.
pub(crate) fn apply_imported(
    parent: &State,
    facts: &ImportedFacts,
) -> Result<ImportedCandidate, TransitionError> {
    let mut overlay = Overlay::new(parent);
    let mut effects = Vec::new();
    let mut token_enables = Vec::new();
    for operation in &facts.operations {
        match operation {
            ImportedOperation::Create {
                identity,
                initial_token,
            } => {
                let portal = portal(&overlay)?;
                let PortalState::AwaitingCreation(expected) = portal else {
                    return Err(TransitionError::PortalAlreadyCreated);
                };
                if expected != *identity {
                    return Err(TransitionError::PortalIdentityMismatch);
                }
                if portal_address(identity.zone_id) != identity.portal {
                    return Err(TransitionError::PortalAddressMismatch);
                }
                if initial_token.token != identity.initial_token {
                    return Err(TransitionError::InitialTokenMismatch);
                }
                overlay.set(
                    StateKey::Portal,
                    Some(StateValue::Portal(PortalState::Created {
                        identity: *identity,
                        bounceback_gas: 0,
                        deposit: Cursor::ZERO,
                        settlement: Settlement::ZERO,
                    })),
                );
                enable_token(&mut overlay, initial_token)?;
                token_enables.push(initial_token.clone());
            }
            ImportedOperation::UpdateBouncebackGas(gas) => {
                update_bounceback_gas(&mut overlay, *gas)?;
            }
            ImportedOperation::EnableToken(enable) => {
                enable_token(&mut overlay, enable)?;
                token_enables.push(enable.clone());
            }
            ImportedOperation::AppendDeposit(deposit) => {
                append_deposit(&mut overlay, deposit, &mut effects)?;
            }
            ImportedOperation::SubmitBatch(input) => {
                submit_batch(&mut overlay, input, &mut effects)?
            }
            ImportedOperation::ProcessWithdrawals(input) => {
                process_withdrawals(&mut overlay, input, &mut effects, &mut token_enables)?
            }
            ImportedOperation::ClaimPortalRefund(input) => {
                claim_refund(&mut overlay, *input, RefundSide::Portal, &mut effects)?
            }
        }
    }
    let delta = overlay.finish();
    let mut state = parent.clone();
    state
        .apply(&delta)
        .map_err(|_| TransitionError::CorruptState)?;
    Ok(ImportedCandidate {
        state,
        effects,
        delta,
        token_enables,
        block_hash: facts.block_hash,
        block_number: facts.block_number,
    })
}
/// Add a newly enabled Portal token in its pre-Zone phase.
fn enable_token(overlay: &mut Overlay<'_>, enable: &TokenEnable) -> Result<(), TransitionError> {
    if !matches!(portal(overlay)?, PortalState::Created { .. }) {
        return Err(TransitionError::PortalNotCreated);
    }
    if overlay.get(&StateKey::Token(enable.token)).is_some() {
        return Err(TransitionError::TokenAlreadyEnabled(enable.token));
    }
    overlay.set(
        StateKey::Token(enable.token),
        Some(StateValue::Token(TokenState::pending())),
    );
    Ok(())
}

/// Update the authenticated Portal bounceback gas configuration.
fn update_bounceback_gas(overlay: &mut Overlay<'_>, gas: u64) -> Result<(), TransitionError> {
    let PortalState::Created {
        identity,
        deposit,
        settlement,
        ..
    } = portal(overlay)?
    else {
        return Err(TransitionError::PortalNotCreated);
    };
    overlay.set(
        StateKey::Portal,
        Some(StateValue::Portal(PortalState::Created {
            identity,
            bounceback_gas: gas,
            deposit,
            settlement,
        })),
    );
    Ok(())
}

/// Append an ordinary Portal deposit to the imported queue.
fn append_deposit(
    overlay: &mut Overlay<'_>,
    input: &OrdinaryDeposit,
    effects: &mut Vec<Effect>,
) -> Result<(), TransitionError> {
    if input.tempo_refund_recipient.is_zero() {
        return Err(TransitionError::ZeroRefundRecipient);
    }
    let PortalState::Created {
        identity,
        bounceback_gas,
        deposit,
        settlement,
    } = portal(overlay)?
    else {
        return Err(TransitionError::PortalNotCreated);
    };
    let mut token = token(overlay, input.token)?;
    let number = deposit
        .number
        .checked_add(1)
        .ok_or(TransitionError::Overflow)?;
    let hash = ordinary_deposit_hash(input, deposit.hash);
    let id = DepositId::new(identity.portal, number).ok_or(TransitionError::Overflow)?;
    if overlay.get(&StateKey::Deposit(id)).is_some() {
        return Err(TransitionError::DepositCollision);
    }
    token.accounting.deposits = token
        .accounting
        .deposits
        .checked_add(U256::from(input.amount))
        .ok_or(TransitionError::Overflow)?;
    overlay.set(StateKey::Token(input.token), Some(StateValue::Token(token)));
    overlay.set(
        StateKey::Deposit(id),
        Some(StateValue::Deposit(DepositOwner::Ordinary(input.clone()))),
    );
    overlay.set(
        StateKey::Portal,
        Some(StateValue::Portal(PortalState::Created {
            identity,
            bounceback_gas,
            deposit: Cursor { hash, number },
            settlement,
        })),
    );
    effects.push(Effect::DepositAppended {
        id,
        queue_hash: hash,
    });
    Ok(())
}

/// Advance Portal settlement for one authenticated submitted batch.
fn submit_batch(
    overlay: &mut Overlay<'_>,
    input: &BatchSubmission,
    effects: &mut Vec<Effect>,
) -> Result<(), TransitionError> {
    let PortalState::Created {
        identity,
        bounceback_gas,
        deposit,
        mut settlement,
    } = portal(overlay)?
    else {
        return Err(TransitionError::PortalNotCreated);
    };
    let next = settlement
        .batch_index
        .checked_add(1)
        .ok_or(TransitionError::Overflow)?;
    let id = BatchId::new(identity.zone_id, next).ok_or(TransitionError::Overflow)?;
    let StateValue::Batch(BatchState::Finalized {
        boundary,
        first_withdrawal,
        count,
        queue_hash,
    }) = overlay
        .get(&StateKey::Batch(id))
        .cloned()
        .ok_or(TransitionError::OwnerMismatch)?
    else {
        return Err(TransitionError::OwnerMismatch);
    };
    if input.tempo_block != boundary.tempo_block
        || input.previous_block != boundary.first_parent
        || input.next_block != boundary.final_block
        || input.previous_deposit != boundary.first_deposit
        || input.next_deposit != boundary.final_deposit
        || input.withdrawal_queue_hash != queue_hash
        || input.next_zone_height != U256::from(boundary.zone_height)
        || settlement.block_hash != boundary.first_parent
        || settlement.submitted_deposit != boundary.first_deposit
        || input.next_zone_height <= settlement.zone_height
        || boundary.final_deposit.number > deposit.number
        || (count != 0 && queue_hash == WITHDRAWAL_TERMINATOR)
    {
        return Err(TransitionError::CommitmentMismatch);
    }
    let queue_index = if count == 0 {
        overlay.set(StateKey::Batch(id), None);
        NO_QUEUE_INDEX
    } else {
        let index = settlement.queue_tail;
        settlement.queue_tail = index
            .checked_add(U256::ONE)
            .ok_or(TransitionError::Overflow)?;
        overlay.set(
            StateKey::Batch(id),
            Some(StateValue::Batch(BatchState::Submitted {
                boundary,
                first_withdrawal,
                count,
                queue_hash,
                next_ordinal: 0,
                logical_queue_index: index,
            })),
        );
        index
    };
    settlement.batch_index = next;
    settlement.block_hash = boundary.final_block;
    settlement.tempo_block = boundary.tempo_block;
    settlement.submitted_deposit = boundary.final_deposit;
    settlement.zone_height = U256::from(boundary.zone_height);
    overlay.set(
        StateKey::Portal,
        Some(StateValue::Portal(PortalState::Created {
            identity,
            bounceback_gas,
            deposit,
            settlement,
        })),
    );
    effects.push(Effect::BatchSubmitted {
        id,
        queue_index,
        processed_deposit_hash: boundary.final_deposit.hash,
        final_block_hash: boundary.final_block,
        queue_hash,
        processed_deposit_number: boundary.final_deposit.number,
    });
    Ok(())
}

/// Consume the next submitted Portal withdrawal prefix and its outcomes.
fn process_withdrawals(
    overlay: &mut Overlay<'_>,
    input: &WithdrawalProcessing,
    effects: &mut Vec<Effect>,
    token_enables: &mut Vec<TokenEnable>,
) -> Result<(), TransitionError> {
    if input.withdrawals.len() != input.outcomes.len() {
        return Err(TransitionError::WithdrawalOutcomeCountMismatch);
    }
    if input.withdrawals.is_empty() {
        return Ok(());
    }
    let PortalState::Created {
        identity,
        deposit: _,
        mut settlement,
        ..
    } = portal(overlay)?
    else {
        return Err(TransitionError::PortalNotCreated);
    };
    let queue_len = checked_queue_len(settlement.queue_head, settlement.queue_tail)?;
    if queue_len.is_zero() {
        return Err(TransitionError::OwnerMismatch);
    }
    let (id, batch) = overlay
        .range(
            std::ops::Bound::Included(StateKey::Batch(
                BatchId::new(identity.zone_id, 1).expect("nonzero lower bound"),
            )),
            std::ops::Bound::Included(StateKey::Batch(
                BatchId::new(identity.zone_id, u64::MAX).expect("nonzero upper bound"),
            )),
        )
        .find_map(|(key, value)| match (key, value) {
            (StateKey::Batch(id), StateValue::Batch(batch)) => Some((id, batch.clone())),
            _ => None,
        })
        .ok_or(TransitionError::OwnerMismatch)?;
    let BatchState::Submitted {
        boundary,
        first_withdrawal,
        count,
        queue_hash,
        next_ordinal,
        logical_queue_index,
    } = batch
    else {
        return Err(TransitionError::OwnerMismatch);
    };
    if logical_queue_index != settlement.queue_head {
        return Err(TransitionError::OwnerMismatch);
    }
    let supplied = u64::try_from(input.withdrawals.len()).map_err(|_| TransitionError::Overflow)?;
    let next = next_ordinal
        .checked_add(supplied)
        .ok_or(TransitionError::Overflow)?;
    if next > count {
        return Err(TransitionError::CommitmentMismatch);
    }
    let folded = input
        .withdrawals
        .iter()
        .rev()
        .fold(input.remaining_queue, |hash, value| {
            withdrawal_hash(value, hash)
        });
    if folded != queue_hash || (next == count) != input.remaining_queue.is_zero() {
        return Err(TransitionError::CommitmentMismatch);
    }
    for (offset, (supplied_value, outcome)) in
        input.withdrawals.iter().zip(&input.outcomes).enumerate()
    {
        let index = first_withdrawal
            .checked_add(next_ordinal)
            .and_then(|v| v.checked_add(u64::try_from(offset).ok()?))
            .ok_or(TransitionError::Overflow)?;
        let wid = WithdrawalId {
            zone_id: identity.zone_id,
            index,
        };
        let StateValue::Withdrawal(WithdrawalOwner::Finalized { data, origin }) = overlay
            .get(&StateKey::Withdrawal(wid))
            .cloned()
            .ok_or(TransitionError::OwnerMismatch)?
        else {
            return Err(TransitionError::OwnerMismatch);
        };
        if &data != supplied_value {
            return Err(TransitionError::CommitmentMismatch);
        }
        let mut withdrawal_token = token(overlay, data.token)?;
        let terminal_effect = match (origin, outcome) {
            (
                WithdrawalOrigin::User { fallback },
                WithdrawalOutcome::UserDelivered { operations },
            ) => {
                require_held_fallback(overlay, fallback, wid, data.token, data.amount)?;
                if data.gas_limit == 0 && !operations.is_empty() {
                    return Err(TransitionError::OwnerMismatch);
                }
                apply_callback_operations(overlay, operations, effects, token_enables)?;
                withdrawal_token = token(overlay, data.token)?;
                withdrawal_token.accounting.withdrawals = withdrawal_token
                    .accounting
                    .withdrawals
                    .checked_sub(U256::from(data.amount))
                    .ok_or(TransitionError::Underflow)?;
                overlay.set(StateKey::Fallback(fallback), None);
                Effect::UserWithdrawalProcessed {
                    to: data.to,
                    sender_tag: data.sender_tag,
                    token: data.token,
                    amount: data.amount,
                    callback_success: true,
                }
            }
            (WithdrawalOrigin::User { fallback }, WithdrawalOutcome::UserBounced) => {
                require_held_fallback(overlay, fallback, wid, data.token, data.amount)?;
                let nonce = fallback.nonce;
                let member = DepositOwner::BounceBack {
                    withdrawal: wid,
                    token: data.token,
                    fallback_nonce: nonce,
                    amount: data.amount,
                };
                let PortalState::Created {
                    identity: pi,
                    bounceback_gas: bg,
                    deposit: pc,
                    settlement: ps,
                } = portal(overlay)?
                else {
                    unreachable!()
                };
                let number = pc.number.checked_add(1).ok_or(TransitionError::Overflow)?;
                let bounce = BounceBackDeposit {
                    token: data.token,
                    fallback_nonce: nonce,
                    amount: data.amount,
                };
                let hash = bounceback_deposit_hash(bounce, pc.hash);
                let did = DepositId::new(pi.portal, number).ok_or(TransitionError::Overflow)?;
                if overlay.get(&StateKey::Deposit(did)).is_some() {
                    return Err(TransitionError::DepositCollision);
                }
                overlay.set(StateKey::Deposit(did), Some(StateValue::Deposit(member)));
                overlay.set(
                    StateKey::Fallback(fallback),
                    Some(StateValue::Fallback(FallbackState::Queued {
                        withdrawal: wid,
                        token: data.token,
                        amount: data.amount,
                        deposit: did,
                    })),
                );
                overlay.set(
                    StateKey::Portal,
                    Some(StateValue::Portal(PortalState::Created {
                        identity: pi,
                        bounceback_gas: bg,
                        deposit: Cursor { hash, number },
                        settlement: ps,
                    })),
                );
                effects.push(Effect::BounceBackAppended {
                    fallback_nonce: nonce.get(),
                    token: data.token,
                    amount: data.amount,
                    id: did,
                    queue_hash: hash,
                });
                Effect::UserWithdrawalProcessed {
                    to: data.to,
                    sender_tag: data.sender_tag,
                    token: data.token,
                    amount: data.amount,
                    callback_success: false,
                }
            }
            (
                WithdrawalOrigin::FailedDeposit { deposit: _ },
                WithdrawalOutcome::FailedDepositPaid { collected_fee },
            ) => {
                let max_fee = bounceback_fee(
                    current_bounceback_gas(overlay)?,
                    input.base_fee,
                    data.amount,
                )
                .ok_or(TransitionError::Overflow)?;
                if *collected_fee != 0 && *collected_fee != max_fee {
                    return Err(TransitionError::CommitmentMismatch);
                }
                withdrawal_token.accounting.deposits = withdrawal_token
                    .accounting
                    .deposits
                    .checked_sub(U256::from(data.amount))
                    .ok_or(TransitionError::Underflow)?;
                Effect::FailedDepositRefunded {
                    recipient: data.to,
                    token: data.token,
                    amount: data.amount - *collected_fee,
                    fee: *collected_fee,
                    pending: false,
                }
            }
            (
                WithdrawalOrigin::FailedDeposit { deposit: failed },
                WithdrawalOutcome::FailedDepositPending { collected_fee },
            ) => {
                let max_fee = bounceback_fee(
                    current_bounceback_gas(overlay)?,
                    input.base_fee,
                    data.amount,
                )
                .ok_or(TransitionError::Overflow)?;
                if *collected_fee != 0 && *collected_fee != max_fee {
                    return Err(TransitionError::CommitmentMismatch);
                }
                withdrawal_token.accounting.deposits = withdrawal_token
                    .accounting
                    .deposits
                    .checked_sub(U256::from(*collected_fee))
                    .ok_or(TransitionError::Underflow)?;
                let refund = PortalRefundId {
                    token: data.token,
                    recipient: data.to,
                    deposit: failed,
                };
                if overlay.get(&StateKey::PortalRefund(refund)).is_some() {
                    return Err(TransitionError::OwnerMismatch);
                }
                overlay.set(
                    StateKey::PortalRefund(refund),
                    Some(StateValue::PortalRefund(RefundCredit {
                        amount: data.amount - *collected_fee,
                    })),
                );
                Effect::FailedDepositRefunded {
                    recipient: data.to,
                    token: data.token,
                    amount: data.amount - *collected_fee,
                    fee: *collected_fee,
                    pending: true,
                }
            }
            _ => return Err(TransitionError::OwnerMismatch),
        };
        overlay.set(
            StateKey::Token(data.token),
            Some(StateValue::Token(withdrawal_token)),
        );
        overlay.set(StateKey::Withdrawal(wid), None);
        effects.push(terminal_effect);
    }
    if next == count {
        overlay.set(StateKey::Batch(id), None);
        settlement.queue_head = settlement
            .queue_head
            .checked_add(U256::ONE)
            .ok_or(TransitionError::Overflow)?;
    } else {
        overlay.set(
            StateKey::Batch(id),
            Some(StateValue::Batch(BatchState::Submitted {
                boundary,
                first_withdrawal,
                count,
                queue_hash: input.remaining_queue,
                next_ordinal: next,
                logical_queue_index,
            })),
        );
    }
    let PortalState::Created {
        identity,
        bounceback_gas,
        deposit,
        ..
    } = portal(overlay)?
    else {
        return Err(TransitionError::PortalNotCreated);
    };
    overlay.set(
        StateKey::Portal,
        Some(StateValue::Portal(PortalState::Created {
            identity,
            bounceback_gas,
            deposit,
            settlement,
        })),
    );
    Ok(())
}

/// Read the current Portal bounce-back gas after any earlier callback operations.
fn current_bounceback_gas(overlay: &Overlay<'_>) -> Result<u64, TransitionError> {
    let PortalState::Created { bounceback_gas, .. } = portal(overlay)? else {
        return Err(TransitionError::PortalNotCreated);
    };
    Ok(bounceback_gas)
}

/// Apply Portal operations emitted by a successful withdrawal callback.
fn apply_callback_operations(
    overlay: &mut Overlay<'_>,
    operations: &[PortalCallbackOperation],
    effects: &mut Vec<Effect>,
    token_enables: &mut Vec<TokenEnable>,
) -> Result<(), TransitionError> {
    for operation in operations {
        match operation {
            PortalCallbackOperation::AppendDeposit(deposit) => {
                append_deposit(overlay, deposit, effects)?
            }
            PortalCallbackOperation::ClaimRefund(claim) => {
                claim_refund(overlay, *claim, RefundSide::Portal, effects)?
            }
            PortalCallbackOperation::EnableToken(enable) => {
                enable_token(overlay, enable)?;
                token_enables.push(enable.clone());
            }
            PortalCallbackOperation::UpdateBouncebackGas(gas) => {
                update_bounceback_gas(overlay, *gas)?
            }
        }
    }
    Ok(())
}

/// Return the number of submitted batches awaiting withdrawal processing.
fn checked_queue_len(head: U256, tail: U256) -> Result<U256, TransitionError> {
    tail.checked_sub(head).ok_or(TransitionError::Underflow)
}

/// Require a user withdrawal's fallback collateral to remain held.
fn require_held_fallback(
    overlay: &Overlay<'_>,
    fallback: FallbackId,
    withdrawal: WithdrawalId,
    token: Address,
    amount: u128,
) -> Result<(), TransitionError> {
    match overlay.get(&StateKey::Fallback(fallback)) {
        Some(StateValue::Fallback(FallbackState::Held {
            withdrawal: actual_withdrawal,
            token: actual_token,
            amount: actual_amount,
        })) if *actual_withdrawal == withdrawal
            && *actual_token == token
            && *actual_amount == amount =>
        {
            Ok(())
        }
        _ => Err(TransitionError::OwnerMismatch),
    }
}
