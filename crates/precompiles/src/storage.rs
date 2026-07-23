//! Anchor coordination and packed-storage compatibility shared by the zone EVM database adapter
//! and the native `TempoState` precompile.

use alloc::{rc::Rc, string::String};
use core::{cell::Cell, fmt};

use alloy_primitives::{Address, B256, keccak256};
use alloy_sol_types::SolValue;
use revm::{context::result::AnyError, precompile::PrecompileError};
use thiserror::Error;

pub(crate) use tempo_precompiles::storage::*;
use zone_primitives::constants::PORTAL_IS_SEQUENCER_SLOT;

/// L1 storage access needed by the anchored Zone database and native precompiles.
pub trait L1StorageReader: Clone + Send + Sync + 'static {
    /// Read `account[slot]` at `block_number` on Tempo L1.
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> core::result::Result<B256, L1StateError>;
}

/// Execution-local access to Tempo L1 storage at a single finalized block number.
///
/// The **anchor** is the Tempo L1 block number against which every external storage read in the
/// current transaction attempt must be resolved. It corresponds to the checkpoint maintained in
/// Zone state by the native `TempoState` precompile. The anchor starts unset and is selected in one
/// of two ways:
///
/// - For an ordinary transaction, the first L1 read selects the `tempoBlockNumber` loaded from the
///   chosen Zone state.
/// - During `advanceTempo`, the native `TempoState` precompile governs advancement: it first
///   validates that the submitted header is the direct child of the stored checkpoint, then
///   advances the anchor to that child before any reads at the new checkpoint.
///
/// Once selected, the anchor is immutable for the transaction attempt. Reads at another block,
/// advancement after a parent-anchor read, and duplicate or non-contiguous advancement are
/// rejected. The Zone EVM resets the anchor after every transaction attempt, including failed and
/// noncommitting simulations.
///
/// Clones share the selected anchor and provider handle so the Zone database adapter, `TempoState`,
/// and other native precompiles enforce one view of L1 state. A new `L1State` must be created for
/// each EVM execution context; it must not be shared across independent EVMs.
#[derive(Clone)]
pub struct L1State<P> {
    /// Tempo block number selected for the current transaction attempt.
    anchor: Rc<Cell<Option<u64>>>,
    /// Underlying cache/RPC-backed reader for storage at an explicit Tempo block number.
    provider: P,
    /// Zone portal whose L1 state defines the active sequencer set.
    portal_address: Address,
}

impl<P> L1State<P> {
    /// Creates execution-local L1 state backed by `provider` for `portal_address`.
    pub fn new(provider: P, portal_address: Address) -> Self {
        Self {
            anchor: Rc::new(Cell::new(None)),
            provider,
            portal_address,
        }
    }

    /// Clears the selected anchor after the current transaction attempt completes.
    pub fn reset_anchor(&self) {
        self.anchor.set(None);
    }

    /// Returns the anchor selected for the current transaction, if any.
    pub fn get_anchor(&self) -> Option<u64> {
        self.anchor.get()
    }

    /// Returns the ZonePortal configured for this execution-local L1 state.
    pub(crate) const fn portal(&self) -> Address {
        self.portal_address
    }

    fn set_anchor(&self, new: u64) -> Result<(), L1StateError> {
        match self.get_anchor() {
            None => {
                self.anchor.set(Some(new));
                Ok(())
            }
            Some(current) if current == new => Ok(()),
            Some(current) => Err(L1StateError::AnchorConflict { current, new }),
        }
    }

    /// Selects the child anchor after `TempoState.finalizeTempo` validates the header transition.
    ///
    /// Advancement is valid only before any L1 read has selected an anchor and only from a parent
    /// to its direct child.
    pub fn advance_anchor(&self, from: u64, to: u64) -> Result<(), L1StateError> {
        if from.checked_add(1) != Some(to) {
            return Err(L1StateError::AdvanceTempoConflict { from, to });
        } else if let Some(current) = self.get_anchor() {
            return Err(L1StateError::AnchorConflict { current, new: to });
        }

        self.anchor.set(Some(to));
        Ok(())
    }
}

impl<P: L1StorageReader> L1State<P> {
    /// Reads L1 storage after selecting or validating `block_number` as this transaction's anchor.
    pub fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> Result<B256, L1StateError> {
        self.set_anchor(block_number)?;
        self.provider.read_l1_storage(account, slot, block_number)
    }

    /// Return whether `account` belongs to the active sequencer set at `block_number`.
    pub fn is_active_sequencer(
        &self,
        account: Address,
        block_number: u64,
    ) -> Result<bool, L1StateError> {
        let slot = keccak256((account, PORTAL_IS_SEQUENCER_SLOT).abi_encode());
        Ok(self.read_l1_storage(self.portal_address, slot, block_number)? != B256::ZERO)
    }
}

impl<P> fmt::Debug for L1State<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("L1State")
            .field("anchor", &self.get_anchor())
            .field("portal_address", &self.portal_address)
            .finish_non_exhaustive()
    }
}

/// Failure to read or advance execution-local Tempo L1 state.
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
    #[error("Tempo L1 read at anchor {new} conflicts with currently selected anchor {current}")]
    AnchorConflict {
        /// Anchor already selected for this transaction.
        current: u64,
        /// Anchor requested by the new read.
        new: u64,
    },
    /// Tempo advancement was non-contiguous or happened after an anchor was selected.
    #[error("cannot advance Tempo L1 anchor from {from} to {to}")]
    AdvanceTempoConflict {
        /// Parent Tempo block number.
        from: u64,
        /// Requested child Tempo block number.
        to: u64,
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

    fn read(l1: &L1State<MockL1Reader>, anchor: u64) -> Result<B256, L1StateError> {
        l1.read_l1_storage(Address::ZERO, B256::ZERO, anchor)
    }

    #[test]
    fn l1_state_rejects_advance_after_parent_read() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        read(&l1, 10).unwrap();
        assert!(l1.advance_anchor(10, 11).is_err());
    }

    #[test]
    fn l1_state_accepts_reads_at_advanced_anchor() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        l1.advance_anchor(10, 11).unwrap();
        read(&l1, 11).unwrap();
        assert_eq!(l1.get_anchor(), Some(11));
    }

    #[test]
    fn l1_state_clones_reject_reads_at_different_anchors() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        let clone = l1.clone();
        read(&l1, 10).unwrap();
        assert!(read(&clone, 11).is_err());
    }

    #[test]
    fn l1_state_rejects_duplicate_advance() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        l1.advance_anchor(10, 11).unwrap();
        assert!(l1.advance_anchor(11, 12).is_err());
    }

    #[test]
    fn l1_state_rejects_non_contiguous_advance() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        assert!(l1.advance_anchor(10, 12).is_err());
        assert_eq!(l1.get_anchor(), None);
    }

    #[test]
    fn l1_state_reset_allows_a_new_anchor() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        read(&l1, 10).unwrap();
        l1.reset_anchor();
        l1.advance_anchor(10, 11).unwrap();
        assert_eq!(l1.get_anchor(), Some(11));
    }

    #[test]
    fn sequencer_membership_uses_requested_anchor() {
        let portal = Address::repeat_byte(0x11);
        let current = Address::repeat_byte(0x22);
        let next = Address::repeat_byte(0x33);
        let reader = MockL1Reader::default();
        reader.seed_active_sequencer(portal, 7, current);
        reader.seed_active_sequencer(portal, 8, next);
        let l1 = L1State::new(reader, portal);

        assert!(l1.is_active_sequencer(current, 7).unwrap());
        assert!(!l1.is_active_sequencer(next, 7).unwrap());
        l1.reset_anchor();
        assert!(l1.is_active_sequencer(next, 8).unwrap());
        assert!(!l1.is_active_sequencer(current, 8).unwrap());
    }
}
