//! Durable bridge accounting derived from authenticated protocol effects.

pub(crate) mod effects;

use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use crate::l2::TokenAccountingEvidence;

/// One user's independently derived entitlement to one Zone token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct AccountKey {
    pub(crate) token: Address,
    pub(crate) account: Address,
}

impl AccountKey {
    pub(crate) const fn new(token: Address, account: Address) -> Self {
        Self { token, account }
    }
}

/// Aggregate circulating and in-flight liabilities for one token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenState {
    /// Token was authenticated through Portal creation or enablement.
    pub(crate) enabled: bool,
    /// Sum of all nonzero account entitlements.
    pub(crate) account_total: U256,
    /// Liability from deposits authenticated on Tempo but not yet reflected on the Zone.
    pub(crate) pending_deposits: U256,
    /// Liability from withdrawals accepted on the Zone but not yet settled on Tempo.
    pub(crate) pending_withdrawals: U256,
    /// Liability from refunds parked on Tempo but not yet claimed.
    pub(crate) pending_refunds: U256,
}

impl TokenState {
    /// Total amount the Portal must currently cover.
    pub(crate) fn liability(self) -> Result<U256, AccountingError> {
        self.account_total
            .checked_add(self.pending_deposits)
            .and_then(|value| value.checked_add(self.pending_withdrawals))
            .and_then(|value| value.checked_add(self.pending_refunds))
            .ok_or(AccountingError::Overflow)
    }

    fn is_empty(self) -> bool {
        !self.enabled
            && self.account_total.is_zero()
            && self.pending_deposits.is_zero()
            && self.pending_withdrawals.is_zero()
            && self.pending_refunds.is_zero()
    }
}

/// One independently authenticated accounting change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Effect {
    EnableToken {
        token: Address,
    },
    Credit {
        key: AccountKey,
        amount: U256,
    },
    Debit {
        key: AccountKey,
        amount: U256,
    },
    Transfer {
        token: Address,
        from: Address,
        to: Address,
        amount: U256,
    },
    /// `increase` grows `pending_deposits` when true, otherwise settles it.
    PendingDeposit {
        token: Address,
        amount: U256,
        increase: bool,
    },
    /// `increase` grows `pending_withdrawals` when true, otherwise settles it.
    PendingWithdrawal {
        token: Address,
        amount: U256,
        increase: bool,
    },
    /// `increase` grows `pending_refunds` when true, otherwise settles it.
    PendingRefund {
        token: Address,
        amount: U256,
        increase: bool,
    },
}

/// Previous values changed by one Zone block, sufficient for deterministic unwind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BlockDelta {
    pub(super) accounts: Vec<(AccountKey, Option<U256>)>,
    pub(super) tokens: Vec<(Address, Option<TokenState>)>,
}

/// Current independently derived bridge accounting state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct State {
    accounts: BTreeMap<AccountKey, U256>,
    tokens: BTreeMap<Address, TokenState>,
}

impl State {
    pub(crate) fn accounts(&self) -> impl Iterator<Item = (AccountKey, U256)> + '_ {
        self.accounts.iter().map(|(&key, &value)| (key, value))
    }

    pub(crate) fn tokens(&self) -> impl Iterator<Item = (Address, TokenState)> + '_ {
        self.tokens.iter().map(|(&token, &state)| (token, state))
    }

    pub(crate) fn account(&self, key: AccountKey) -> Option<U256> {
        self.accounts.get(&key).copied()
    }

    pub(crate) fn token(&self, token: Address) -> Option<TokenState> {
        self.tokens.get(&token).copied()
    }

    /// Build state from raw rows, dropping zero entries and asserting aggregate consistency.
    pub(crate) fn from_rows(
        accounts: impl IntoIterator<Item = (AccountKey, U256)>,
        tokens: impl IntoIterator<Item = (Address, TokenState)>,
    ) -> Result<Self, AccountingError> {
        let state = Self {
            accounts: accounts
                .into_iter()
                .filter(|(_, value)| !value.is_zero())
                .collect(),
            tokens: tokens
                .into_iter()
                .filter(|(_, value)| !value.is_empty())
                .collect(),
        };
        state.validate_aggregates()?;
        Ok(state)
    }

    /// Apply ordered effects atomically and return their undo delta.
    ///
    /// Mutates in place and tracks each touched entry's prior value; on any
    /// failure that same tracked delta is used to roll the mutation back
    /// before returning the error, so no upfront full-state clone is needed
    /// to guarantee `self` is unchanged when this returns `Err`.
    pub(crate) fn apply(&mut self, effects: &[Effect]) -> Result<BlockDelta, AccountingError> {
        let mut accounts = BTreeMap::new();
        let mut tokens = BTreeMap::new();

        let result = effects
            .iter()
            .try_for_each(|effect| self.apply_effect(*effect, &mut accounts, &mut tokens))
            .and_then(|()| self.validate_aggregates());

        if let Err(error) = result {
            for (&key, &previous) in &accounts {
                write_optional(&mut self.accounts, key, previous);
            }
            for (&token, &previous) in &tokens {
                write_optional(&mut self.tokens, token, previous);
            }
            return Err(error);
        }

        Ok(BlockDelta {
            accounts: accounts.into_iter().collect(),
            tokens: tokens.into_iter().collect(),
        })
    }

    /// Restore the state that preceded an applied block.
    pub(crate) fn unwind(&mut self, delta: BlockDelta) -> Result<(), AccountingError> {
        for (key, previous) in delta.accounts {
            write_optional(&mut self.accounts, key, previous);
        }
        for (token, previous) in delta.tokens {
            write_optional(&mut self.tokens, token, previous);
        }
        self.validate_aggregates()
    }

    /// Verify exact post-state balances and supplies for the supplied observations.
    pub(crate) fn verify_zone_state(
        &self,
        observed: &[TokenAccountingEvidence],
    ) -> Result<(), AccountingError> {
        for evidence in observed {
            for (&account, &actual) in &evidence.balances {
                let key = AccountKey::new(evidence.token, account);
                let expected = self.accounts.get(&key).copied().unwrap_or_default();
                if actual != expected {
                    return Err(AccountingError::BalanceMismatch {
                        key,
                        expected,
                        actual,
                    });
                }
            }
            let expected = self
                .tokens
                .get(&evidence.token)
                .map(|state| state.account_total)
                .unwrap_or_default();
            if evidence.total_supply != expected {
                return Err(AccountingError::SupplyMismatch {
                    token: evidence.token,
                    expected,
                    actual: evidence.total_supply,
                });
            }
        }
        Ok(())
    }

    /// Verify exact L1 Portal custody against every supplied token liability.
    pub(crate) fn verify_portal_balances(
        &self,
        balances: impl IntoIterator<Item = (Address, U256)>,
    ) -> Result<(), AccountingError> {
        for (token, available) in balances {
            let required = self
                .tokens
                .get(&token)
                .copied()
                .unwrap_or_default()
                .liability()?;
            if available < required {
                return Err(AccountingError::CollateralShortfall {
                    token,
                    required,
                    available,
                });
            }
        }
        Ok(())
    }

    fn apply_effect(
        &mut self,
        effect: Effect,
        accounts: &mut BTreeMap<AccountKey, Option<U256>>,
        tokens: &mut BTreeMap<Address, Option<TokenState>>,
    ) -> Result<(), AccountingError> {
        match effect {
            Effect::EnableToken { token } => {
                tokens
                    .entry(token)
                    .or_insert_with(|| self.tokens.get(&token).copied());
                let mut state = self.tokens.get(&token).copied().unwrap_or_default();
                state.enabled = true;
                self.write_token(token, state);
                Ok(())
            }
            Effect::Credit { key, amount } => {
                self.change_account(key, amount, true, accounts, tokens)
            }
            Effect::Debit { key, amount } => {
                self.change_account(key, amount, false, accounts, tokens)
            }
            Effect::Transfer {
                token,
                from,
                to,
                amount,
            } => {
                self.change_account(
                    AccountKey::new(token, from),
                    amount,
                    false,
                    accounts,
                    tokens,
                )?;
                self.change_account(AccountKey::new(token, to), amount, true, accounts, tokens)
            }
            Effect::PendingDeposit {
                token,
                amount,
                increase,
            } => self.change_liability(token, amount, increase, tokens, |state| {
                &mut state.pending_deposits
            }),
            Effect::PendingWithdrawal {
                token,
                amount,
                increase,
            } => self.change_liability(token, amount, increase, tokens, |state| {
                &mut state.pending_withdrawals
            }),
            Effect::PendingRefund {
                token,
                amount,
                increase,
            } => self.change_liability(token, amount, increase, tokens, |state| {
                &mut state.pending_refunds
            }),
        }
    }

    fn change_account(
        &mut self,
        key: AccountKey,
        amount: U256,
        increase: bool,
        accounts: &mut BTreeMap<AccountKey, Option<U256>>,
        tokens: &mut BTreeMap<Address, Option<TokenState>>,
    ) -> Result<(), AccountingError> {
        accounts
            .entry(key)
            .or_insert_with(|| self.accounts.get(&key).copied());
        tokens
            .entry(key.token)
            .or_insert_with(|| self.tokens.get(&key.token).copied());

        let current = self.accounts.get(&key).copied().unwrap_or_default();
        let next = checked_change(current, amount, increase)?;
        write_nonzero(&mut self.accounts, key, next);

        let mut token = self.tokens.get(&key.token).copied().unwrap_or_default();
        token.account_total = checked_change(token.account_total, amount, increase)?;
        self.write_token(key.token, token);
        Ok(())
    }

    fn change_liability(
        &mut self,
        token: Address,
        amount: U256,
        increase: bool,
        previous: &mut BTreeMap<Address, Option<TokenState>>,
        field: impl FnOnce(&mut TokenState) -> &mut U256,
    ) -> Result<(), AccountingError> {
        previous
            .entry(token)
            .or_insert_with(|| self.tokens.get(&token).copied());
        let mut state = self.tokens.get(&token).copied().unwrap_or_default();
        let value = field(&mut state);
        *value = checked_change(*value, amount, increase)?;
        self.write_token(token, state);
        Ok(())
    }

    fn write_token(&mut self, token: Address, state: TokenState) {
        if state.is_empty() {
            self.tokens.remove(&token);
        } else {
            self.tokens.insert(token, state);
        }
    }

    fn validate_aggregates(&self) -> Result<(), AccountingError> {
        let mut totals = BTreeMap::<Address, U256>::new();
        for (key, balance) in &self.accounts {
            let total = totals.entry(key.token).or_default();
            *total = total
                .checked_add(*balance)
                .ok_or(AccountingError::Overflow)?;
        }
        for (&token, state) in &self.tokens {
            let actual = totals.remove(&token).unwrap_or_default();
            if state.account_total != actual {
                return Err(AccountingError::AggregateMismatch {
                    token,
                    expected: actual,
                    actual: state.account_total,
                });
            }
        }
        if let Some((token, expected)) = totals.into_iter().next() {
            return Err(AccountingError::AggregateMismatch {
                token,
                expected,
                actual: U256::ZERO,
            });
        }
        Ok(())
    }
}

fn checked_change(value: U256, amount: U256, increase: bool) -> Result<U256, AccountingError> {
    if increase {
        value.checked_add(amount).ok_or(AccountingError::Overflow)
    } else {
        value.checked_sub(amount).ok_or(AccountingError::Underflow)
    }
}

fn write_nonzero<K: Ord>(values: &mut BTreeMap<K, U256>, key: K, value: U256) {
    if value.is_zero() {
        values.remove(&key);
    } else {
        values.insert(key, value);
    }
}

fn write_optional<K: Ord, V>(values: &mut BTreeMap<K, V>, key: K, value: Option<V>) {
    match value {
        Some(value) => {
            values.insert(key, value);
        }
        None => {
            values.remove(&key);
        }
    }
}

/// Deterministic accounting or externally observed invariant failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AccountingError {
    #[error("accounting arithmetic overflow")]
    Overflow,
    #[error("accounting arithmetic underflow")]
    Underflow,
    #[error("account {key:?} balance mismatch: expected {expected}, observed {actual}")]
    BalanceMismatch {
        key: AccountKey,
        expected: U256,
        actual: U256,
    },
    #[error("token {token} supply mismatch: expected {expected}, observed {actual}")]
    SupplyMismatch {
        token: Address,
        expected: U256,
        actual: U256,
    },
    #[error("token {token} aggregate mismatch: expected {expected}, stored {actual}")]
    AggregateMismatch {
        token: Address,
        expected: U256,
        actual: U256,
    },
    #[error("token {token} collateral shortfall: required {required}, available {available}")]
    CollateralShortfall {
        token: Address,
        required: U256,
        available: U256,
    },
}

#[cfg(test)]
mod tests;
