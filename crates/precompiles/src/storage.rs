//! Tempo L1 storage access shared by the Zone EVM database adapter and native precompiles.

use alloc::{rc::Rc, string::String};
use core::{cell::Cell, fmt};

use alloy_primitives::{Address, B256, keccak256};
use alloy_sol_types::SolValue;
use revm::{context::result::AnyError, precompile::PrecompileError};
use thiserror::Error;
use zone_primitives::constants::PORTAL_IS_SEQUENCER_SLOT;

pub(crate) use tempo_precompiles::storage::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempoAnchor {
    /// Tempo block number selected for L1 reads.
    pub block_number: u64,
    /// State root committed by the selected Tempo header.
    pub state_root: B256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum L1ReadMode {
    Authenticated,
    Unauthenticated,
}

/// L1 storage access needed by the anchored Zone database and native precompiles.
pub trait L1StorageReader: Clone + Send + Sync + 'static {
    /// Resolve storage at `block_number`, authenticating it against `state_root` when supplied.
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
        state_root: Option<B256>,
    ) -> core::result::Result<B256, L1StateError>;
}

/// Execution-local access to Tempo L1 storage at a finalized checkpoint.
///
/// [`TempoState`](crate::TempoState) persists the canonical Tempo block number and state root in
/// Zone state. The transaction-local anchor only ensures that every L1 read within one transaction
/// uses the same checkpoint. During `advanceTempo`, it also makes the newly validated checkpoint
/// available to the database overlay before the journaled `TempoState` write is committed.
///
/// The Zone EVM clears the anchor after every transaction attempt. Clones share it so the database
/// adapter and native precompiles enforce one transaction-local view of L1 state.
#[derive(Clone)]
pub struct L1State<P> {
    /// Tempo block number and state root selected for the current transaction.
    anchor: Rc<Cell<Option<TempoAnchor>>>,
    /// Whether reads must be authenticated against the anchor's state root.
    mode: L1ReadMode,
    /// Underlying cache/RPC-backed reader for storage at an explicit Tempo block number.
    provider: P,
    /// Zone portal whose L1 state defines the active sequencer set.
    portal_address: Address,
}

impl<P> L1State<P> {
    /// Creates execution-local L1 state that requires authenticated storage proofs.
    pub fn authenticated(provider: P, portal_address: Address) -> Self {
        Self {
            anchor: Rc::new(Cell::new(None)),
            mode: L1ReadMode::Authenticated,
            provider,
            portal_address,
        }
    }

    /// Creates execution-local L1 state that uses unauthenticated RPC storage reads.
    pub fn unauthenticated(provider: P, portal_address: Address) -> Self {
        Self {
            anchor: Rc::new(Cell::new(None)),
            mode: L1ReadMode::Unauthenticated,
            provider,
            portal_address,
        }
    }

    /// Clears the transaction-local anchor.
    pub fn reset_anchor(&self) {
        self.anchor.set(None);
    }

    /// Returns the selected Tempo anchor, if any.
    pub fn get_anchor(&self) -> Option<TempoAnchor> {
        self.anchor.get()
    }

    /// Returns the ZonePortal configured for this execution-local L1 state.
    pub(crate) const fn portal(&self) -> Address {
        self.portal_address
    }

    pub(crate) fn select_anchor(&self, new: TempoAnchor) -> Result<(), L1StateError> {
        match self.get_anchor() {
            None => {
                self.anchor.set(Some(new));
                Ok(())
            }
            Some(current) if current == new => Ok(()),
            Some(current) => Err(L1StateError::AnchorConflict { current, new }),
        }
    }
}

impl<P: L1StorageReader> L1State<P> {
    /// Reads L1 storage at `anchor`, requiring all reads in the transaction to use the same anchor.
    pub fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        anchor: TempoAnchor,
    ) -> Result<B256, L1StateError> {
        self.select_anchor(anchor)?;
        let state_root = match self.mode {
            L1ReadMode::Authenticated => Some(anchor.state_root),
            L1ReadMode::Unauthenticated => None,
        };
        self.provider
            .read_l1_storage(account, slot, anchor.block_number, state_root)
    }

    /// Return whether `account` belongs to the active sequencer set at `anchor`.
    pub fn is_active_sequencer(
        &self,
        account: Address,
        anchor: TempoAnchor,
    ) -> Result<bool, L1StateError> {
        let slot = keccak256((account, PORTAL_IS_SEQUENCER_SLOT).abi_encode());
        Ok(self.read_l1_storage(self.portal_address, slot, anchor)? != B256::ZERO)
    }
}

impl<P> fmt::Debug for L1State<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("L1State")
            .field("anchor", &self.get_anchor())
            .field("mode", &self.mode)
            .field("portal_address", &self.portal_address)
            .finish_non_exhaustive()
    }
}

/// Failure to read execution-local Tempo L1 state at one consistent checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum L1StateError {
    /// The underlying L1 provider could not resolve storage at the requested block.
    #[error(
        "Tempo L1 storage unavailable account={account} slot={slot} block={block_number}: {reason}"
    )]
    StorageUnavailable {
        /// Tempo account whose storage was requested.
        account: Address,
        /// Requested storage slot.
        slot: B256,
        /// Tempo block number at which storage was requested.
        block_number: u64,
        /// Provider failure diagnostic.
        reason: String,
    },
    /// An L1 read disagreed with the anchor already selected for this transaction.
    #[error("Tempo L1 read at anchor {new:?} conflicts with currently selected anchor {current:?}")]
    AnchorConflict {
        /// Anchor already selected for this transaction.
        current: TempoAnchor,
        /// Anchor requested by the new read.
        new: TempoAnchor,
    },
}

impl L1StateError {
    /// Returns whether this is an underlying storage-provider failure.
    pub const fn is_storage_unavailable(&self) -> bool {
        matches!(self, Self::StorageUnavailable { .. })
    }
}

impl From<L1StateError> for PrecompileError {
    fn from(error: L1StateError) -> Self {
        Self::FatalAny(AnyError::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockL1Reader;

    fn anchor(block_number: u64) -> TempoAnchor {
        TempoAnchor {
            block_number,
            state_root: B256::with_last_byte(block_number as u8),
        }
    }

    fn read(l1: &L1State<MockL1Reader>, anchor: TempoAnchor) -> Result<B256, L1StateError> {
        l1.read_l1_storage(Address::ZERO, B256::ZERO, anchor)
    }

    #[test]
    fn l1_state_rejects_new_checkpoint_after_parent_read() {
        let l1 = L1State::unauthenticated(MockL1Reader::default(), Address::ZERO);
        read(&l1, anchor(10)).unwrap();
        assert!(l1.select_anchor(anchor(11)).is_err());
    }

    #[test]
    fn l1_state_accepts_reads_at_selected_anchor() {
        let l1 = L1State::unauthenticated(MockL1Reader::default(), Address::ZERO);
        let anchor = anchor(11);
        l1.select_anchor(anchor).unwrap();
        read(&l1, anchor).unwrap();
        assert_eq!(l1.get_anchor(), Some(anchor));
    }

    #[test]
    fn l1_state_clones_reject_reads_at_different_anchors() {
        let l1 = L1State::unauthenticated(MockL1Reader::default(), Address::ZERO);
        let clone = l1.clone();
        read(&l1, anchor(10)).unwrap();
        assert!(read(&clone, anchor(11)).is_err());
    }

    #[test]
    fn l1_state_rejects_replacing_selected_anchor() {
        let l1 = L1State::unauthenticated(MockL1Reader::default(), Address::ZERO);
        l1.select_anchor(anchor(11)).unwrap();
        assert!(l1.select_anchor(anchor(12)).is_err());
    }

    #[test]
    fn l1_state_reset_allows_a_new_anchor() {
        let l1 = L1State::unauthenticated(MockL1Reader::default(), Address::ZERO);
        read(&l1, anchor(10)).unwrap();
        l1.reset_anchor();
        let anchor = anchor(11);
        l1.select_anchor(anchor).unwrap();
        assert_eq!(l1.get_anchor(), Some(anchor));
    }

    #[test]
    fn anchor_rejects_reads_at_a_different_checkpoint() {
        let reader = MockL1Reader::default();
        let l1 = L1State::authenticated(reader.clone(), Address::ZERO);
        let current = anchor(11);
        let new = anchor(10);
        read(&l1, current).unwrap();
        assert!(matches!(
            read(&l1, new),
            Err(L1StateError::AnchorConflict {
                current: observed_current,
                new: observed_new,
            })
            if observed_current == current && observed_new == new
        ));
        assert_eq!(reader.storage_requests().len(), 1);
    }

    #[test]
    fn matching_state_root_is_forwarded_to_the_reader() {
        let reader = MockL1Reader::default();
        let l1 = L1State::authenticated(reader.clone(), Address::ZERO);
        let state_root = B256::with_last_byte(1);
        let anchor = TempoAnchor {
            block_number: 10,
            state_root,
        };

        read(&l1, anchor).unwrap();
        assert_eq!(reader.storage_state_roots(), vec![Some(state_root)]);
    }

    #[test]
    fn reset_allows_a_new_authenticated_anchor() {
        let l1 = L1State::authenticated(MockL1Reader::default(), Address::ZERO);
        let current_root = B256::with_last_byte(2);
        let new_root = B256::with_last_byte(4);
        l1.select_anchor(TempoAnchor {
            block_number: 10,
            state_root: current_root,
        })
        .unwrap();
        l1.reset_anchor();

        l1.select_anchor(TempoAnchor {
            block_number: 10,
            state_root: new_root,
        })
        .unwrap();
        assert_eq!(l1.get_anchor().unwrap().state_root, new_root);
    }

    #[test]
    fn unauthenticated_state_ignores_checkpoint_binding() {
        let reader = MockL1Reader::default();
        let l1 = L1State::unauthenticated(reader.clone(), Address::ZERO);
        let anchor = anchor(10);

        read(&l1, anchor).unwrap();
        assert_eq!(reader.storage_state_roots(), vec![None]);
    }

    #[test]
    fn sequencer_membership_uses_requested_anchor() {
        let portal = Address::repeat_byte(0x11);
        let current = Address::repeat_byte(0x22);
        let next = Address::repeat_byte(0x33);
        let reader = MockL1Reader::default();
        reader.seed_active_sequencer(portal, 7, current);
        reader.seed_active_sequencer(portal, 8, next);
        let l1 = L1State::unauthenticated(reader, portal);

        assert!(l1.is_active_sequencer(current, anchor(7)).unwrap());
        assert!(!l1.is_active_sequencer(next, anchor(7)).unwrap());
        l1.reset_anchor();
        assert!(l1.is_active_sequencer(next, anchor(8)).unwrap());
        assert!(!l1.is_active_sequencer(current, anchor(8)).unwrap());
    }
}
