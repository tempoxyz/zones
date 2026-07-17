//! Shared state for the Zone EVM's anchored Tempo database adapter.

use alloc::rc::Rc;
use core::{cell::RefCell, fmt};

use alloy_primitives::{Address, B256, U256};
use revm::precompile::PrecompileError;
use tempo_precompiles::tip20::tip20_slots;
use tempo_primitives::TempoAddressExt;
use thiserror::Error;

pub(crate) use tempo_precompiles::storage::*;

/// L1 storage access needed by the anchored Zone database and `TempoState` reads.
pub trait L1StorageReader: Clone + Send + Sync + 'static {
    /// Read `account[slot]` at `block_number` on Tempo L1.
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> core::result::Result<B256, PrecompileError>;
}

/// Invalid operation for the current anchor phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("invalid anchor operation {operation:?} in phase {phase:?}")]
pub struct L1AnchorError {
    /// Attempted operation.
    pub operation: L1AnchorOperation,
    /// Phase in which the operation was attempted.
    pub phase: L1AnchorPhase,
}

/// Operation applied to the execution-local anchor state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L1AnchorOperation {
    /// Initialize from the checkpoint stored in selected Zone state.
    Initialize { anchor: u64 },
    /// Observe external Tempo state at an anchor.
    Read { anchor: u64 },
    /// Advance from a parent Tempo block to its direct child.
    Advance { from: u64, to: u64 },
}

/// Current execution-local Tempo anchor phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum L1AnchorPhase {
    /// The selected Zone state's checkpoint has not been loaded yet.
    #[default]
    Uninitialized,
    /// Execution is still at the parent anchor.
    Parent {
        /// Parent Tempo block number.
        anchor: u64,
        /// Whether external Tempo state was observed at this anchor.
        has_read_l1: bool,
    },
    /// The required system transaction advanced this execution to the child anchor.
    Advanced {
        /// Parent Tempo block number.
        from: u64,
        /// Child Tempo block number.
        to: u64,
        /// Whether external Tempo state was observed after advancement.
        has_read_l1: bool,
    },
}

impl L1AnchorPhase {
    /// Returns the anchor used by reads in this phase, if initialized.
    pub const fn current(self) -> Option<u64> {
        match self {
            Self::Uninitialized => None,
            Self::Parent { anchor, .. } => Some(anchor),
            Self::Advanced { to, .. } => Some(to),
        }
    }

    /// Returns whether this phase has completed the required advancement.
    pub const fn is_advanced(self) -> bool {
        matches!(self, Self::Advanced { .. })
    }

    const fn parent(anchor: u64) -> Self {
        Self::Parent {
            anchor,
            has_read_l1: false,
        }
    }

    const fn advanced(from: u64, to: u64) -> Self {
        Self::Advanced {
            from,
            to,
            has_read_l1: false,
        }
    }

    const fn with_l1_read(self) -> Self {
        match self {
            Self::Parent { anchor, .. } => Self::Parent {
                anchor,
                has_read_l1: true,
            },
            Self::Advanced { from, to, .. } => Self::Advanced {
                from,
                to,
                has_read_l1: true,
            },
            Self::Uninitialized => Self::Uninitialized,
        }
    }

    fn apply(self, operation: L1AnchorOperation) -> Result<Self, L1AnchorError> {
        let invalid = || L1AnchorError {
            operation,
            phase: self,
        };

        match operation {
            L1AnchorOperation::Initialize { anchor: new } => match self {
                Self::Uninitialized => Ok(Self::parent(new)),
                Self::Parent { anchor: prev, .. } if prev == new => Ok(self),
                Self::Advanced { to, .. } if to == new => Ok(self),
                _ => Err(invalid()),
            },
            L1AnchorOperation::Read { anchor: new } => match self {
                Self::Uninitialized => Ok(Self::parent(new).with_l1_read()),
                Self::Parent { anchor: prev, .. } if prev == new => Ok(self.with_l1_read()),
                Self::Advanced { to, .. } if to == new => Ok(self.with_l1_read()),
                _ => Err(invalid()),
            },
            L1AnchorOperation::Advance { from, to } => {
                if from.checked_add(1) != Some(to) {
                    return Err(invalid());
                }
                match self {
                    Self::Uninitialized => Ok(Self::advanced(from, to)),
                    Self::Parent {
                        anchor,
                        has_read_l1: false,
                    } if anchor == from => Ok(Self::advanced(from, to)),
                    _ => Err(invalid()),
                }
            }
        }
    }
}

/// The execution-local Tempo anchor used by the database adapter.
#[derive(Clone, Default)]
pub struct L1AnchorController {
    state: Rc<RefCell<L1AnchorPhase>>,
}

impl fmt::Debug for L1AnchorController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnchorController")
            .field("phase", &self.phase())
            .finish()
    }
}

impl L1AnchorController {
    fn apply(&self, operation: L1AnchorOperation) -> Result<L1AnchorPhase, L1AnchorError> {
        let mut phase = self.state.borrow_mut();
        let next = phase.apply(operation)?;
        *phase = next;
        Ok(next)
    }

    /// Returns the current phase.
    pub fn phase(&self) -> L1AnchorPhase {
        *self.state.borrow()
    }

    /// Restores a previous controller snapshot.
    pub fn restore(&self, snapshot: L1AnchorPhase) {
        *self.state.borrow_mut() = snapshot;
    }

    /// Returns the anchor used by reads in the current phase, if initialized.
    pub fn current(&self) -> Option<u64> {
        self.phase().current()
    }

    /// Initializes the controller from the selected Zone state.
    pub fn initialize(&self, anchor: u64) -> Result<u64, L1AnchorError> {
        let phase = self.apply(L1AnchorOperation::Initialize { anchor })?;
        Ok(phase.current().expect("initialization produces an anchor"))
    }

    /// Records a successful external Tempo state read at `anchor`.
    pub fn observe_read(&self, anchor: u64) -> Result<(), L1AnchorError> {
        self.apply(L1AnchorOperation::Read { anchor })?;
        Ok(())
    }

    /// Advances the execution-local anchor after `finalizeTempo` validates the next header.
    pub fn begin_advance(&self, from: u64, to: u64) -> Result<(), L1AnchorError> {
        self.apply(L1AnchorOperation::Advance { from, to })?;
        Ok(())
    }
}

/// Returns whether a slot is mirrored from Tempo L1.
pub fn is_mirrored_slot(address: Address, key: U256) -> bool {
    address == tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS
        || is_tip20_policy_id_slot(address, key)
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
        assert_eq!(
            controller.phase(),
            L1AnchorPhase::Advanced {
                from: 10,
                to: 11,
                has_read_l1: true
            }
        );
    }

    #[test]
    fn controller_rejects_reads_at_wrong_anchor() {
        let controller = L1AnchorController::default();
        controller.initialize(10).unwrap();
        assert!(controller.observe_read(11).is_err());
    }

    #[test]
    fn controller_rejects_duplicate_advance() {
        let controller = L1AnchorController::default();
        controller.begin_advance(10, 11).unwrap();
        assert!(controller.begin_advance(11, 12).is_err());
    }

    #[test]
    fn controller_snapshot_restores_phase() {
        let controller = L1AnchorController::default();
        controller.initialize(10).unwrap();
        let snapshot = controller.phase();
        controller.begin_advance(10, 11).unwrap();
        controller.restore(snapshot);
        assert_eq!(
            controller.phase(),
            L1AnchorPhase::Parent {
                anchor: 10,
                has_read_l1: false
            }
        );
    }
}
