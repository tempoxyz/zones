//! Anchor coordination and packed-storage compatibility shared by the zone EVM database adapter
//! and the native `TempoState` precompile.

use alloc::{
    rc::Rc,
    string::{String, ToString},
};
use core::{cell::Cell, fmt};

use alloy_primitives::{Address, B256, U256};
use revm::{context::result::AnyError, precompile::PrecompileError};
use tempo_precompiles::{
    error::TempoPrecompileError, zone_factory::ZonePortalStorage as ZonePortal,
};
use thiserror::Error;

use crate::tempo_state::TempoState;

pub(crate) use tempo_precompiles::storage::*;

/// Complete commitment used for Tempo storage reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempoAnchor {
    block_number: u64,
    state_root: B256,
}

impl TempoAnchor {
    /// Creates an immutable Tempo L1 anchor.
    pub const fn new(block_number: u64, state_root: B256) -> Self {
        Self {
            block_number,
            state_root,
        }
    }

    /// Returns the anchored Tempo L1 block number.
    pub const fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Returns the anchored Tempo L1 state root.
    pub const fn state_root(&self) -> B256 {
        self.state_root
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub const fn dummy(block_number: u64) -> Self {
        Self::new(block_number, B256::with_last_byte(block_number as u8))
    }
}

/// L1 storage access needed by the anchored Zone database and native precompiles.
pub trait L1StorageReader: Clone + Send + Sync + 'static {
    fn read_l1_storage(
        &self,
        anchor: TempoAnchor,
        account: Address,
        slot: B256,
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
    anchor: Rc<Cell<Option<TempoAnchor>>>,
    /// Underlying cache/RPC-backed reader for storage at an explicit Tempo block number.
    provider: P,
    /// ZonePortal read through the L1 provider by explicit storage operations.
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
    pub fn get_anchor(&self) -> Option<TempoAnchor> {
        self.anchor.get()
    }

    /// Returns the configured ZonePortal address.
    pub const fn portal(&self) -> Address {
        self.portal_address
    }

    fn set_anchor(&self, new: TempoAnchor) -> Result<(), L1StateError> {
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
    pub fn advance_anchor(&self, from: TempoAnchor, to: TempoAnchor) -> Result<(), L1StateError> {
        if from.block_number().checked_add(1) != Some(to.block_number()) {
            return Err(L1StateError::AdvanceTempoConflict {
                from: from.block_number(),
                to: to.block_number(),
            });
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
        anchor: TempoAnchor,
    ) -> Result<B256, L1StateError> {
        self.set_anchor(anchor)?;
        self.provider.read_l1_storage(anchor, account, slot)
    }

    /// Reads and decodes a typed slot from an L1 account at the active anchor.
    pub fn read_l1<T: Storable>(&self, slot: &Slot<T>) -> tempo_precompiles::Result<T> {
        let storage = L1Storage {
            l1: self,
            account: slot.address(),
        };
        T::load(&storage, slot.slot(), slot.ctx())
    }

    /// Selects and reads a typed slot from the configured ZonePortal at the active anchor.
    ///
    /// The callback only exposes the portal for selecting a handler; the selected value is always
    /// resolved through [`Self::read_l1`] rather than the local EVM journal.
    pub fn read_portal<T: Storable>(
        &self,
        select_slot: impl for<'a> FnOnce(&'a ZonePortal) -> &'a Slot<T>,
    ) -> tempo_precompiles::Result<T> {
        let portal = ZonePortal::new(self.portal_address);
        self.read_l1(select_slot(&portal))
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
    #[error("Tempo L1 read at anchor {new:?} conflicts with currently selected anchor {current:?}")]
    AnchorConflict {
        /// Anchor already selected for this transaction.
        current: TempoAnchor,
        /// Anchor requested by the new read.
        new: TempoAnchor,
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

/// Read-only [`StorageOps`] adapter for one L1 account.
///
/// This lets typed precompile storage handlers decode L1 slots without inserting the fetched
/// values into the EVM journal. Reads use the transaction's selected anchor, falling back to the
/// checkpoint stored in [`TempoState`] on the first read; writes are always rejected.
struct L1Storage<'a, P> {
    /// Execution-local provider and anchor coordination shared by all L1 reads.
    l1: &'a L1State<P>,
    /// L1 account whose storage slots are exposed through [`StorageOps`].
    account: Address,
}

impl<P: L1StorageReader> StorageOps for L1Storage<'_, P> {
    fn load(&self, slot: U256) -> tempo_precompiles::Result<U256> {
        let anchor = match self.l1.get_anchor() {
            Some(anchor) => anchor,
            None => TempoState::new().anchor()?,
        };
        self.l1
            .read_l1_storage(self.account, slot.into(), anchor)
            .map(Into::into)
            .map_err(|err| TempoPrecompileError::Fatal(err.to_string()))
    }

    fn store(&mut self, _slot: U256, _value: U256) -> tempo_precompiles::Result<()> {
        Err(TempoPrecompileError::Fatal(
            "L1 storage is read-only".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockL1Reader;

    fn read(l1: &L1State<MockL1Reader>, anchor: u64) -> Result<B256, L1StateError> {
        l1.read_l1_storage(Address::ZERO, B256::ZERO, TempoAnchor::dummy(anchor))
    }

    #[test]
    fn l1_state_rejects_advance_after_parent_read() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        read(&l1, 10).unwrap();
        assert!(
            l1.advance_anchor(TempoAnchor::dummy(10), TempoAnchor::dummy(11))
                .is_err()
        );
    }

    #[test]
    fn l1_state_accepts_reads_at_advanced_anchor() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        l1.advance_anchor(TempoAnchor::dummy(10), TempoAnchor::dummy(11))
            .unwrap();
        read(&l1, 11).unwrap();
        assert_eq!(l1.get_anchor().map(|a| a.block_number()), Some(11));
    }

    #[test]
    fn l1_state_clones_reject_reads_at_different_anchors() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        let clone = l1.clone();
        read(&l1, 10).unwrap();
        assert!(read(&clone, 11).is_err());
    }

    #[test]
    fn l1_state_rejects_same_block_with_different_state_root() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        let first = TempoAnchor::new(10, B256::with_last_byte(1));
        let conflicting = TempoAnchor::new(10, B256::with_last_byte(2));
        l1.read_l1_storage(Address::ZERO, B256::ZERO, first)
            .unwrap();
        assert!(
            l1.read_l1_storage(Address::ZERO, B256::ZERO, conflicting)
                .is_err()
        );
    }

    #[test]
    fn l1_state_rejects_duplicate_advance() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        l1.advance_anchor(TempoAnchor::dummy(10), TempoAnchor::dummy(11))
            .unwrap();
        assert!(
            l1.advance_anchor(TempoAnchor::dummy(11), TempoAnchor::dummy(12),)
                .is_err()
        );
    }

    #[test]
    fn l1_state_rejects_non_contiguous_advance() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        assert!(
            l1.advance_anchor(TempoAnchor::dummy(10), TempoAnchor::dummy(12),)
                .is_err()
        );
        assert_eq!(l1.get_anchor(), None);
    }

    #[test]
    fn l1_state_reset_allows_a_new_anchor() {
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        read(&l1, 10).unwrap();
        l1.reset_anchor();
        l1.advance_anchor(TempoAnchor::dummy(10), TempoAnchor::dummy(11))
            .unwrap();
        assert_eq!(l1.get_anchor().map(|a| a.block_number()), Some(11));
    }
}
