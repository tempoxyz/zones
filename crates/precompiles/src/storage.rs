//! Anchor coordination and packed-storage compatibility shared by the zone EVM database adapter
//! and the native `TempoState` precompile.

use alloc::{rc::Rc, string::String};
use core::{cell::Cell, fmt};

use alloy_primitives::{Address, B256, U256};
use revm::{context::result::AnyError, precompile::PrecompileError};
use tempo_precompiles::tip20::tip20_slots;
use tempo_primitives::TempoAddressExt;
use thiserror::Error;

pub(crate) use tempo_precompiles::storage::*;

/// Failure to read storage from Tempo L1.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct L1StorageError(pub String);

impl From<L1StorageError> for PrecompileError {
    fn from(error: L1StorageError) -> Self {
        Self::FatalAny(AnyError::new(error))
    }
}

/// L1 storage access needed by the anchored Zone database and `TempoState` reads.
pub trait L1StorageReader: Clone + Send + Sync + 'static {
    /// Read `account[slot]` at `block_number` on Tempo L1.
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> core::result::Result<B256, L1StorageError>;
}

/// Invalid selection of a transaction's Tempo L1 anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum L1AnchorError {
    /// An L1 read disagreed with the anchor already selected for this transaction.
    #[error("Tempo L1 read at anchor {observed} conflicts with selected anchor {selected}")]
    Read {
        /// Anchor already selected for this transaction.
        selected: u64,
        /// Anchor requested by the new read.
        observed: u64,
    },
    /// Tempo advancement was non-contiguous or happened after an anchor was selected.
    #[error("cannot advance Tempo L1 anchor from {from} to {to} after selecting {selected:?}")]
    Advance {
        /// Anchor already selected for this transaction, if any.
        selected: Option<u64>,
        /// Parent Tempo block number.
        from: u64,
        /// Requested child Tempo block number.
        to: u64,
    },
}

/// The execution-local Tempo anchor used by the database adapter.
/// The single Tempo L1 anchor selected for the current transaction attempt.
#[derive(Clone, Default)]
pub struct L1AnchorController(Rc<Cell<Option<u64>>>);

impl fmt::Debug for L1AnchorController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnchorController")
            .field("anchor", &self.current())
            .finish()
    }
}

impl L1AnchorController {
    /// Resets the selected anchor for the next transaction attempt.
    pub fn reset(&self) {
        self.0.set(None);
    }

    /// Returns the anchor selected for the current transaction, if any.
    pub fn current(&self) -> Option<u64> {
        self.0.get()
    }

    /// Validates the selected anchor and records an external Tempo state read.
    pub fn observe_read(&self, anchor: u64) -> Result<(), L1AnchorError> {
        match self.current() {
            None => {
                self.0.set(Some(anchor));
                Ok(())
            }
            Some(selected) if selected == anchor => Ok(()),
            Some(selected) => Err(L1AnchorError::Read {
                selected,
                observed: anchor,
            }),
        }
    }

    /// Selects the child anchor after `finalizeTempo` validates a contiguous header.
    pub fn begin_advance(&self, from: u64, to: u64) -> Result<(), L1AnchorError> {
        let selected = self.current();
        if from.checked_add(1) != Some(to) || selected.is_some() {
            return Err(L1AnchorError::Advance { selected, from, to });
        }

        self.0.set(Some(to));
        Ok(())
    }
}

/// Returns whether this is the packed TIP-20 transfer-policy slot.
pub fn is_tip20_policy_id_slot(address: Address, key: U256) -> bool {
    address.is_tip20() && key == tip20_slots::TRANSFER_POLICY_ID
}

/// Replaces the L1-owned transfer-policy field while preserving Zone-local packed fields.
pub fn merge_transfer_policy_id(local_slot: U256, l1_slot: U256) -> U256 {
    let offset_bits = tip20_slots::TRANSFER_POLICY_ID_OFFSET * 8;
    let field_bits = core::mem::size_of::<u64>() * 8;
    let field_mask = ((U256::ONE << field_bits) - U256::ONE) << offset_bits;
    (local_slot & !field_mask) | (l1_slot & field_mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_rejects_advance_after_parent_read() {
        let controller = L1AnchorController::default();
        controller.observe_read(10).unwrap();
        assert!(controller.begin_advance(10, 11).is_err());
    }

    #[test]
    fn controller_accepts_reads_at_advanced_anchor() {
        let controller = L1AnchorController::default();
        controller.begin_advance(10, 11).unwrap();
        controller.observe_read(11).unwrap();
        assert_eq!(controller.current(), Some(11));
    }

    #[test]
    fn controller_rejects_reads_at_wrong_anchor() {
        let controller = L1AnchorController::default();
        controller.observe_read(10).unwrap();
        assert!(controller.observe_read(11).is_err());
    }

    #[test]
    fn controller_rejects_duplicate_advance() {
        let controller = L1AnchorController::default();
        controller.begin_advance(10, 11).unwrap();
        assert!(controller.begin_advance(11, 12).is_err());
    }

    #[test]
    fn controller_rejects_non_contiguous_advance() {
        let controller = L1AnchorController::default();
        assert!(controller.begin_advance(10, 12).is_err());
        assert_eq!(controller.current(), None);
    }

    #[test]
    fn controller_reset_allows_a_new_anchor() {
        let controller = L1AnchorController::default();
        controller.observe_read(10).unwrap();
        controller.reset();
        controller.begin_advance(10, 11).unwrap();
        assert_eq!(controller.current(), Some(11));
    }
}
