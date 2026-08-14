//! Persisted checker state and its typed keys, values, and mutations.

use std::{collections::BTreeMap, num::NonZeroU64, ops::Bound};

use alloy_primitives::{Address, B256, Bytes, U256};
use serde::{Deserialize, Serialize};

use crate::kernel::facts::OrdinaryDeposit;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct PortalIdentity {
    pub(crate) portal: Address,
    pub(crate) zone_id: u32,
    pub(crate) initial_token: Address,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct DepositId {
    pub(crate) portal: Address,
    pub(crate) number: NonZeroU64,
}

impl DepositId {
    pub(crate) fn new(portal: Address, number: u64) -> Option<Self> {
        Some(Self {
            portal,
            number: NonZeroU64::new(number)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct WithdrawalId {
    pub(crate) zone_id: u32,
    pub(crate) index: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct BatchId {
    pub(crate) zone_id: u32,
    pub(crate) index: NonZeroU64,
}

impl BatchId {
    pub(crate) fn new(zone_id: u32, index: u64) -> Option<Self> {
        Some(Self {
            zone_id,
            index: NonZeroU64::new(index)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct FallbackId {
    pub(crate) zone_id: u32,
    pub(crate) nonce: NonZeroU64,
}

impl FallbackId {
    pub(crate) fn new(zone_id: u32, nonce: u64) -> Option<Self> {
        Some(Self {
            zone_id,
            nonce: NonZeroU64::new(nonce)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct PortalRefundId {
    pub(crate) token: Address,
    pub(crate) recipient: Address,
    pub(crate) deposit: DepositId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct InboxRefundId {
    pub(crate) token: Address,
    pub(crate) recipient: Address,
    pub(crate) withdrawal: WithdrawalId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum StateKey {
    Portal,
    Zone,
    Token(Address),
    Deposit(DepositId),
    Withdrawal(WithdrawalId),
    Batch(BatchId),
    Fallback(FallbackId),
    PortalRefund(PortalRefundId),
    InboxRefund(InboxRefundId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    pub(crate) hash: B256,
    pub(crate) number: u64,
}

impl Cursor {
    pub(crate) const ZERO: Self = Self {
        hash: B256::ZERO,
        number: 0,
    };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum PortalState {
    AwaitingCreation(PortalIdentity),
    Created {
        identity: PortalIdentity,
        bounceback_gas: u64,
        deposit: Cursor,
        settlement: Settlement,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Settlement {
    pub(crate) batch_index: u64,
    pub(crate) block_hash: B256,
    pub(crate) tempo_block: u64,
    pub(crate) submitted_deposit: Cursor,
    pub(crate) zone_height: U256,
    pub(crate) queue_head: U256,
    pub(crate) queue_tail: U256,
}

impl Settlement {
    pub(crate) const ZERO: Self = Self {
        batch_index: 0,
        block_hash: B256::ZERO,
        tempo_block: 0,
        submitted_deposit: Cursor::ZERO,
        zone_height: U256::ZERO,
        queue_head: U256::ZERO,
        queue_tail: U256::ZERO,
    };
}

impl PortalState {
    pub(crate) const fn identity(&self) -> PortalIdentity {
        match self {
            Self::AwaitingCreation(identity) | Self::Created { identity, .. } => *identity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ZoneState {
    pub(crate) processed_deposit: Cursor,
    pub(crate) next_withdrawal_index: u64,
    pub(crate) withdrawal_queue_hash: B256,
    pub(crate) withdrawal_batch_index: u64,
    pub(crate) tempo_gas_rate: u128,
    pub(crate) max_withdrawals_per_block: u32,
    pub(crate) last_fallback_nonce: u64,
    pub(crate) batch_start: BatchBoundaryStart,
}

impl Default for ZoneState {
    fn default() -> Self {
        Self {
            processed_deposit: Cursor::ZERO,
            next_withdrawal_index: 0,
            withdrawal_queue_hash: B256::ZERO,
            withdrawal_batch_index: 0,
            tempo_gas_rate: 0,
            max_withdrawals_per_block: 0,
            last_fallback_nonce: 0,
            batch_start: BatchBoundaryStart::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BatchBoundaryStart {
    pub(crate) parent_hash: B256,
    pub(crate) deposit: Cursor,
    pub(crate) withdrawal_index: u64,
}
impl BatchBoundaryStart {
    pub(crate) const ZERO: Self = Self {
        parent_hash: B256::ZERO,
        deposit: Cursor::ZERO,
        withdrawal_index: 0,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TokenPhase {
    PendingZoneEnable,
    ZoneEnabled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenAccounting {
    pub(crate) supply: U256,
    pub(crate) deposits: U256,
    pub(crate) withdrawals: U256,
}

impl TokenAccounting {
    pub(crate) fn collateral(self) -> Option<U256> {
        self.supply
            .checked_add(self.deposits)?
            .checked_add(self.withdrawals)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenState {
    pub(crate) phase: TokenPhase,
    pub(crate) accounting: TokenAccounting,
}

impl TokenState {
    pub(crate) fn pending() -> Self {
        Self {
            phase: TokenPhase::PendingZoneEnable,
            accounting: TokenAccounting::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DepositOwner {
    Ordinary(OrdinaryDeposit),
    BounceBack {
        withdrawal: WithdrawalId,
        token: Address,
        fallback_nonce: NonZeroU64,
        amount: u128,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum WithdrawalOwner {
    PendingFailedDeposit {
        deposit: DepositId,
        token: Address,
        recipient: Address,
        amount: u128,
    },
    PendingUser {
        data: Withdrawal,
        fallback: FallbackId,
    },
    Finalized {
        data: Withdrawal,
        origin: WithdrawalOrigin,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Withdrawal {
    pub(crate) token: Address,
    pub(crate) sender_tag: B256,
    pub(crate) to: Address,
    pub(crate) amount: u128,
    pub(crate) memo: B256,
    pub(crate) gas_limit: u64,
    pub(crate) fallback_nonce: u64,
    pub(crate) callback_data: Bytes,
    pub(crate) encrypted_sender: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum WithdrawalOrigin {
    User { fallback: FallbackId },
    FailedDeposit { deposit: DepositId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BatchBoundary {
    pub(crate) first_parent: B256,
    pub(crate) final_block: B256,
    pub(crate) first_deposit: Cursor,
    pub(crate) final_deposit: Cursor,
    pub(crate) tempo_block: u64,
    pub(crate) zone_height: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BatchState {
    Finalized {
        boundary: BatchBoundary,
        first_withdrawal: u64,
        count: u64,
        queue_hash: B256,
    },
    Submitted {
        boundary: BatchBoundary,
        first_withdrawal: u64,
        count: u64,
        queue_hash: B256,
        next_ordinal: u64,
        logical_queue_index: U256,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FallbackState {
    Held {
        withdrawal: WithdrawalId,
        token: Address,
        amount: u128,
    },
    Queued {
        withdrawal: WithdrawalId,
        token: Address,
        amount: u128,
        deposit: DepositId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RefundCredit {
    pub(crate) amount: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum StateValue {
    Portal(PortalState),
    Zone(ZoneState),
    Token(TokenState),
    Deposit(DepositOwner),
    Withdrawal(WithdrawalOwner),
    Batch(BatchState),
    Fallback(FallbackState),
    PortalRefund(RefundCredit),
    InboxRefund(RefundCredit),
}

impl StateValue {
    pub(crate) fn matches_key(&self, key: &StateKey) -> bool {
        matches!(
            (key, self),
            (StateKey::Portal, Self::Portal(_))
                | (StateKey::Zone, Self::Zone(_))
                | (StateKey::Token(_), Self::Token(_))
                | (StateKey::Deposit(_), Self::Deposit(_))
                | (StateKey::Withdrawal(_), Self::Withdrawal(_))
                | (StateKey::Batch(_), Self::Batch(_))
                | (StateKey::Fallback(_), Self::Fallback(_))
                | (StateKey::PortalRefund(_), Self::PortalRefund(_))
                | (StateKey::InboxRefund(_), Self::InboxRefund(_))
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct State {
    rows: BTreeMap<StateKey, StateValue>,
}

impl State {
    pub(crate) fn awaiting(identity: PortalIdentity) -> Self {
        Self {
            rows: BTreeMap::from([
                (
                    StateKey::Portal,
                    StateValue::Portal(PortalState::AwaitingCreation(identity)),
                ),
                (StateKey::Zone, StateValue::Zone(ZoneState::default())),
            ]),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_pending_withdrawals_for_test(
        identity: PortalIdentity,
        count: u64,
        callback_size: usize,
    ) -> Self {
        let mut state = Self::awaiting(identity);
        for index in 1..=count {
            let withdrawal = WithdrawalId {
                zone_id: identity.zone_id,
                index,
            };
            state.rows.insert(
                StateKey::Withdrawal(withdrawal),
                StateValue::Withdrawal(WithdrawalOwner::PendingUser {
                    data: Withdrawal {
                        token: identity.initial_token,
                        sender_tag: B256::from(U256::from(index)),
                        to: Address::repeat_byte(1),
                        amount: 1,
                        memo: B256::ZERO,
                        gas_limit: 1,
                        fallback_nonce: index,
                        callback_data: Bytes::from(vec![0; callback_size]),
                        encrypted_sender: Bytes::new(),
                    },
                    fallback: FallbackId::new(identity.zone_id, index)
                        .expect("test index is nonzero"),
                }),
            );
        }
        state
    }

    #[cfg(test)]
    pub(super) fn from_rows(
        rows: BTreeMap<StateKey, StateValue>,
    ) -> Result<Self, StateFamilyError> {
        for (key, value) in &rows {
            if !value.matches_key(key) {
                return Err(StateFamilyError { key: *key });
            }
        }
        Ok(Self { rows })
    }

    pub(super) fn rows(&self) -> &BTreeMap<StateKey, StateValue> {
        &self.rows
    }

    pub(crate) fn portal(&self) -> Option<&PortalState> {
        match self.rows.get(&StateKey::Portal) {
            Some(StateValue::Portal(portal)) => Some(portal),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn zone(&self) -> Option<&ZoneState> {
        match self.rows.get(&StateKey::Zone) {
            Some(StateValue::Zone(zone)) => Some(zone),
            _ => None,
        }
    }

    pub(crate) fn token(&self, address: Address) -> Option<&TokenState> {
        match self.rows.get(&StateKey::Token(address)) {
            Some(StateValue::Token(token)) => Some(token),
            _ => None,
        }
    }

    pub(crate) fn tokens(&self) -> impl Iterator<Item = (Address, &TokenState)> {
        self.rows
            .iter()
            .filter_map(|(key, value)| match (key, value) {
                (StateKey::Token(address), StateValue::Token(token)) => Some((*address, token)),
                _ => None,
            })
    }

    pub(crate) fn validate_families(&self) -> Result<(), StateFamilyError> {
        for (key, value) in &self.rows {
            if !value.matches_key(key) {
                return Err(StateFamilyError { key: *key });
            }
        }
        Ok(())
    }

    pub(crate) fn apply(&mut self, delta: &StateDelta) -> Result<(), StateFamilyError> {
        delta.validate()?;
        for (key, value) in &delta.writes {
            match value {
                Some(value) => {
                    self.rows.insert(*key, value.clone());
                }
                None => {
                    self.rows.remove(key);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("state value does not match key family {key:?}")]
pub(crate) struct StateFamilyError {
    pub(crate) key: StateKey,
}

pub(crate) struct Overlay<'a> {
    parent: &'a State,
    writes: BTreeMap<StateKey, Option<StateValue>>,
}

impl<'a> Overlay<'a> {
    pub(crate) fn new(parent: &'a State) -> Self {
        Self {
            parent,
            writes: BTreeMap::new(),
        }
    }

    pub(crate) fn get(&self, key: &StateKey) -> Option<&StateValue> {
        self.writes
            .get(key)
            .map_or_else(|| self.parent.rows.get(key), Option::as_ref)
    }

    pub(crate) fn set(&mut self, key: StateKey, value: Option<StateValue>) {
        debug_assert!(value.as_ref().is_none_or(|value| value.matches_key(&key)));
        self.writes.insert(key, value);
    }

    pub(crate) fn range(
        &self,
        start: Bound<StateKey>,
        end: Bound<StateKey>,
    ) -> impl Iterator<Item = (StateKey, &StateValue)> {
        let mut merged = self
            .parent
            .rows
            .range((start, end))
            .map(|(key, value)| (*key, value))
            .collect::<BTreeMap<_, _>>();
        for (key, value) in self.writes.range((start, end)) {
            match value {
                Some(value) => {
                    merged.insert(*key, value);
                }
                None => {
                    merged.remove(key);
                }
            }
        }
        merged.into_iter()
    }

    pub(crate) fn finish(self) -> StateDelta {
        StateDelta {
            writes: self.writes.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateDelta {
    writes: Vec<(StateKey, Option<StateValue>)>,
}

impl StateDelta {
    pub(crate) fn from_sorted_writes(writes: Vec<(StateKey, Option<StateValue>)>) -> Self {
        Self { writes }
    }

    pub(crate) fn writes(&self) -> &[(StateKey, Option<StateValue>)] {
        &self.writes
    }

    pub(crate) fn validate(&self) -> Result<(), StateFamilyError> {
        let mut previous = None;
        for (key, value) in &self.writes {
            if previous.is_some_and(|previous| previous >= *key)
                || value.as_ref().is_some_and(|value| !value.matches_key(key))
            {
                return Err(StateFamilyError { key: *key });
            }
            previous = Some(*key);
        }
        Ok(())
    }
}
