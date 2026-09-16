//! Execution-local database adapter that overlays finalized Tempo L1 reads while preserving
//! the caller-provided Zone database as the sole canonical state backend.

use std::fmt;

use alloy_primitives::{Address, B256, U256};
use evm2::{
    AnyError, ErrorCode, PendingState,
    bytecode::Bytecode,
    evm::{
        AccountChangeRef, AccountInfo, Database, DynDatabase, StateChangeSink, StateChangeSource,
        StorageChange,
    },
};
use thiserror::Error;
use zone_precompiles::{
    TIP403_REGISTRY_ADDRESS,
    storage::{L1State, L1StateError, L1StorageReader},
    tempo_state::TEMPO_BLOCK_NUMBER_SLOT,
};
use zone_primitives::constants::TEMPO_STATE_ADDRESS;

/// Resolves mirrored L1 reads at the active Tempo anchor and forwards all other database
/// operations to the caller-provided Zone database.
pub struct L1OverlayDB<DB, L1> {
    inner: DB,
    l1: L1State<L1>,
    error: Option<AnyError>,
}

impl<DB, L1> L1OverlayDB<DB, L1> {
    /// Creates an adapter around the caller-provided database.
    pub fn new(inner: DB, l1: L1, portal_address: Address) -> Self {
        Self {
            inner,
            l1: L1State::new(l1, portal_address),
            error: None,
        }
    }

    /// Returns the original caller-provided database.
    pub const fn inner(&self) -> &DB {
        &self.inner
    }

    /// Returns the original caller-provided database mutably.
    pub const fn inner_mut(&mut self) -> &mut DB {
        &mut self.inner
    }

    /// Recovers the original caller-provided database.
    pub fn into_inner(self) -> DB {
        self.inner
    }

    /// Returns the execution-local L1 state shared with L1-backed precompiles.
    pub const fn l1_state(&self) -> &L1State<L1> {
        &self.l1
    }
}

const L1_ERROR: ErrorCode = ErrorCode::new_custom(0).expect("valid custom error code");

impl<DB: DynDatabase, L1: L1StorageReader> L1OverlayDB<DB, L1> {
    fn anchor_dyn(&mut self) -> Result<u64, ErrorCode> {
        if let Some(anchor) = self.l1.get_anchor() {
            return Ok(anchor);
        }

        let value = self
            .inner
            .get_storage(&TEMPO_STATE_ADDRESS, &TEMPO_BLOCK_NUMBER_SLOT)?;
        u64::try_from(value).map_err(|_| {
            self.error = Some(AnyError::new(
                ZoneDbError::<core::convert::Infallible>::AnchorOverflow(value),
            ));
            L1_ERROR
        })
    }

    fn store_l1_error(&mut self, error: L1StateError) -> ErrorCode {
        self.error = Some(AnyError::new(error));
        L1_ERROR
    }
}

impl<DB: DynDatabase, L1: L1StorageReader> DynDatabase for L1OverlayDB<DB, L1> {
    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, ErrorCode> {
        self.inner.get_account(address)
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, ErrorCode> {
        self.inner.get_code_by_hash(code_hash)
    }

    fn get_storage(&mut self, address: &Address, slot: &U256) -> Result<U256, ErrorCode> {
        if *address != TIP403_REGISTRY_ADDRESS {
            return self.inner.get_storage(address, slot);
        }

        let anchor = self.anchor_dyn()?;
        self.l1
            .read_l1_storage_unmetered(*address, B256::from(*slot), anchor)
            .map(Into::into)
            .map_err(|error| self.store_l1_error(error))
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, ErrorCode> {
        self.inner.get_block_hash(number)
    }

    fn error(&mut self, code: ErrorCode) -> AnyError {
        if code == L1_ERROR
            && let Some(error) = self.error.clone()
        {
            return error;
        }
        self.inner.error(code)
    }
}

impl<DB: fmt::Debug, L1> fmt::Debug for L1OverlayDB<DB, L1> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("L1OverlayDB")
            .field("inner", &self.inner)
            .field("l1", &self.l1)
            .finish_non_exhaustive()
    }
}

impl<DB: Database, L1: L1StorageReader> L1OverlayDB<DB, L1> {
    fn anchor(&mut self) -> Result<u64, ZoneDbError<DB::Error>> {
        if let Some(anchor) = self.l1.get_anchor() {
            return Ok(anchor);
        }

        let value = self
            .inner
            .get_storage(&TEMPO_STATE_ADDRESS, &TEMPO_BLOCK_NUMBER_SLOT)
            .map_err(ZoneDbError::Inner)?;
        let anchor = u64::try_from(value).map_err(|_| ZoneDbError::AnchorOverflow(value))?;
        Ok(anchor)
    }

    /// Rejects writes to the L1-mirrored TIP-403 registry.
    pub fn validate_pending_state(
        &self,
        state: &PendingState,
    ) -> Result<(), ZoneDbError<DB::Error>> {
        state.visit(&mut L1WriteGuard(core::marker::PhantomData))
    }
}

pub(crate) fn validate_pending_state(
    state: &PendingState,
) -> Result<(), ZoneDbError<core::convert::Infallible>> {
    state.visit(&mut L1WriteGuard(core::marker::PhantomData))
}

struct L1WriteGuard<E>(core::marker::PhantomData<E>);

impl<E> StateChangeSink for L1WriteGuard<E> {
    type Error = ZoneDbError<E>;

    fn account(&mut self, change: AccountChangeRef<'_>) -> Result<(), Self::Error> {
        if change.address == TIP403_REGISTRY_ADDRESS {
            return Err(ZoneDbError::L1Write {
                address: change.address,
                slot: U256::ZERO,
            });
        }
        Ok(())
    }

    fn storage_wipe(&mut self, address: Address) -> Result<(), Self::Error> {
        if address == TIP403_REGISTRY_ADDRESS {
            return Err(ZoneDbError::L1Write {
                address,
                slot: U256::ZERO,
            });
        }
        Ok(())
    }

    fn storage(&mut self, change: StorageChange) -> Result<(), Self::Error> {
        if change.address == TIP403_REGISTRY_ADDRESS {
            return Err(ZoneDbError::L1Write {
                address: change.address,
                slot: change.key,
            });
        }
        Ok(())
    }
}

impl<DB: Database, L1: L1StorageReader> Database for L1OverlayDB<DB, L1> {
    type Error = ZoneDbError<DB::Error>;

    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner.get_account(address).map_err(ZoneDbError::Inner)
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
        self.inner
            .get_code_by_hash(code_hash)
            .map_err(ZoneDbError::Inner)
    }

    fn get_storage(&mut self, address: &Address, slot: &U256) -> Result<U256, Self::Error> {
        if *address != TIP403_REGISTRY_ADDRESS {
            return self
                .inner
                .get_storage(address, slot)
                .map_err(ZoneDbError::Inner);
        }

        let anchor = self.anchor()?;
        // The EVM already charges this TIP-403 SLOAD; the host-side L1 fetch must not be charged
        // again.
        self.l1
            .read_l1_storage_unmetered(*address, B256::from(*slot), anchor)
            .map(Into::into)
            .map_err(ZoneDbError::L1State)
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, Self::Error> {
        self.inner
            .get_block_hash(number)
            .map_err(ZoneDbError::Inner)
    }
}

/// Database error produced by [`L1OverlayDB`].
#[derive(Debug, Error)]
pub enum ZoneDbError<E> {
    /// Error from the caller-provided database.
    #[error("inner database error: {0}")]
    Inner(#[source] E),
    /// The selected Zone state contains an invalid Tempo anchor.
    #[error("invalid Tempo anchor (does not fit in u64): {0}")]
    AnchorOverflow(U256),
    /// Execution-local Tempo L1 state could not be read or advanced consistently.
    #[error(transparent)]
    L1State(#[from] L1StateError),
    /// A transaction attempted to persist mirrored Tempo-owned state.
    #[error("write to mirrored Tempo storage address={address} slot={slot}")]
    L1Write { address: Address, slot: U256 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use evm2::evm::InMemoryDB;
    use zone_precompiles::test_utils::MockL1Reader as TestL1;

    fn test_db(anchor: u64) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_storage(
            &TEMPO_STATE_ADDRESS,
            &TEMPO_BLOCK_NUMBER_SLOT,
            &U256::from(anchor),
        );
        db
    }

    #[test]
    fn overlays_registry_at_selected_state_anchor() {
        let anchor = 42;
        let slot = U256::from(7);
        let expected = U256::from(99);
        let l1 = TestL1::default();
        l1.insert(TIP403_REGISTRY_ADDRESS, slot, anchor - 1, U256::from(98));
        l1.insert(TIP403_REGISTRY_ADDRESS, slot, anchor, expected);
        let mut db = L1OverlayDB::new(test_db(anchor), l1, Address::ZERO);

        assert_eq!(
            Database::get_storage(&mut db, &TIP403_REGISTRY_ADDRESS, &slot).unwrap(),
            expected
        );
        assert_eq!(db.l1_state().get_anchor(), Some(anchor));
    }

    #[test]
    fn l1_failures_and_read_before_advance_fail_closed() {
        let anchor = 42;
        let slot = U256::from(7);
        let mut failing =
            L1OverlayDB::new(test_db(anchor), TestL1::failing_storage(), Address::ZERO);
        assert!(matches!(
            Database::get_storage(&mut failing, &TIP403_REGISTRY_ADDRESS, &slot),
            Err(ZoneDbError::L1State(L1StateError::StorageUnavailable {
                block_number: 42,
                ..
            }))
        ));

        let reader = TestL1::default();
        reader.insert(TIP403_REGISTRY_ADDRESS, slot, anchor, U256::ONE);
        let mut db = L1OverlayDB::new(test_db(anchor), reader.clone(), Address::ZERO);
        let l1 = db.l1_state().clone();
        assert_eq!(
            Database::get_storage(&mut db, &TIP403_REGISTRY_ADDRESS, &slot).unwrap(),
            U256::ONE
        );
        assert!(l1.advance_anchor(anchor, anchor + 1).is_err());
        assert_eq!(reader.storage_requests().len(), 1);
    }

    #[test]
    fn registry_overlay_is_removed_from_canonical_transition() {
        let (anchor, slot) = (42, U256::from(7));
        let (local, l1_value) = (U256::from(5), U256::from(99));
        let l1 = TestL1::default();
        l1.insert(TIP403_REGISTRY_ADDRESS, slot, anchor, l1_value);
        let mut inner = test_db(anchor);
        inner.insert_account_storage(&TIP403_REGISTRY_ADDRESS, &slot, &local);
        let mut db = L1OverlayDB::new(inner, l1, Address::ZERO);
        let observed = Database::get_storage(&mut db, &TIP403_REGISTRY_ADDRESS, &slot).unwrap();
        assert_eq!(observed, l1_value);

        let mut state = PendingState::default();
        state.insert_storage(TIP403_REGISTRY_ADDRESS, slot, observed, observed);
        db.validate_pending_state(&state).unwrap();

        let mut inner = db.into_inner();
        assert_eq!(
            Database::get_storage(&mut inner, &TIP403_REGISTRY_ADDRESS, &slot).unwrap(),
            local
        );
    }

    #[test]
    fn transaction_reset_clears_anchor() {
        let (anchor, slot) = (42, U256::from(7));
        let l1 = TestL1::default();
        l1.insert(TIP403_REGISTRY_ADDRESS, slot, anchor, U256::from(7));
        let mut db = L1OverlayDB::new(test_db(anchor), l1, Address::ZERO);

        Database::get_storage(&mut db, &TIP403_REGISTRY_ADDRESS, &slot).unwrap();
        assert_eq!(db.l1_state().get_anchor(), Some(anchor));

        db.l1_state().reset_transaction_state();

        assert_eq!(db.l1_state().get_anchor(), None);
    }

    #[test]
    fn ordinary_storage_is_local_and_inner_database_is_recoverable() {
        let address = Address::repeat_byte(0x11);
        let slot = U256::from(3);
        let value = U256::from(5);
        let mut inner = test_db(1);
        inner.insert_account_storage(&address, &slot, &value);
        let mut db = L1OverlayDB::new(inner, TestL1::default(), Address::ZERO);

        assert_eq!(
            Database::get_storage(&mut db, &address, &slot).unwrap(),
            value
        );
        let mut inner: InMemoryDB = db.into_inner();
        assert_eq!(
            Database::get_storage(&mut inner, &address, &slot).unwrap(),
            value
        );
    }
}
