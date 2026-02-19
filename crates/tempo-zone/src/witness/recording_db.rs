//! Recording database wrapper for witness generation.
//!
//! Wraps any revm [`Database`] to record which accounts and storage slots are
//! accessed during EVM execution. After execution, the recorded accesses can be
//! used to generate MPT proofs for the [`ZoneStateWitness`].

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, B256, U256};
use reth_errors::ProviderError;
use reth_revm::Database;
use revm::state::{AccountInfo, Bytecode};

/// Shared handle to recorded state accesses.
///
/// This handle can be cloned before the `RecordingDatabase` is consumed by
/// `State::builder().with_database(...)`, allowing the caller to retrieve
/// recorded accesses after execution is complete.
#[derive(Clone, Default)]
pub struct RecordedAccesses {
    inner: Arc<Mutex<RecordedAccessesInner>>,
}

/// Interior data for recorded accesses.
#[derive(Default)]
struct RecordedAccessesInner {
    /// Accounts whose `basic()` was called.
    accounts: BTreeSet<Address>,
    /// Storage slots read per account.
    storage: BTreeMap<Address, BTreeSet<U256>>,
    /// Code hashes that were looked up.
    code_hashes: BTreeSet<B256>,
    /// Block hashes that were looked up.
    block_hashes: BTreeSet<u64>,
}

impl RecordedAccesses {
    /// Create a new empty handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot of all accessed account addresses.
    pub fn accessed_accounts(&self) -> BTreeSet<Address> {
        self.inner.lock().expect("poisoned").accounts.clone()
    }

    /// Returns a snapshot of all accessed storage slots, grouped by account.
    pub fn accessed_storage(&self) -> BTreeMap<Address, BTreeSet<U256>> {
        self.inner.lock().expect("poisoned").storage.clone()
    }

    /// Returns a snapshot of all accessed code hashes.
    pub fn accessed_code_hashes(&self) -> BTreeSet<B256> {
        self.inner.lock().expect("poisoned").code_hashes.clone()
    }

    /// Returns a snapshot of all accessed block numbers (for BLOCKHASH).
    pub fn accessed_block_hashes(&self) -> BTreeSet<u64> {
        self.inner.lock().expect("poisoned").block_hashes.clone()
    }

    /// Record an account access.
    fn record_account(&self, address: Address) {
        self.inner.lock().expect("poisoned").accounts.insert(address);
    }

    /// Record a storage slot access.
    fn record_storage(&self, address: Address, slot: U256) {
        let mut inner = self.inner.lock().expect("poisoned");
        inner.accounts.insert(address);
        inner.storage.entry(address).or_default().insert(slot);
    }

    /// Record a code hash access.
    fn record_code_hash(&self, code_hash: B256) {
        self.inner
            .lock()
            .expect("poisoned")
            .code_hashes
            .insert(code_hash);
    }

    /// Record a block hash access.
    fn record_block_hash(&self, number: u64) {
        self.inner
            .lock()
            .expect("poisoned")
            .block_hashes
            .insert(number);
    }
}

/// A database wrapper that records all state accesses via a shared handle.
///
/// Delegates all reads to the inner database but logs every `basic()`,
/// `storage()`, and `code_by_hash()` call to a [`RecordedAccesses`] handle.
///
/// The handle can be cloned before the database is consumed, allowing
/// the caller to retrieve the accesses after execution.
///
/// # Usage
///
/// ```ignore
/// let accesses = RecordedAccesses::new();
/// let recording_db = RecordingDatabase::new(inner_db, accesses.clone());
/// // ... wrap in State, execute blocks ...
/// let accounts = accesses.accessed_accounts();
/// let storage = accesses.accessed_storage();
/// ```
pub struct RecordingDatabase<DB> {
    inner: DB,
    accesses: RecordedAccesses,
}

impl<DB> std::fmt::Debug for RecordingDatabase<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingDatabase")
            .field("accesses", &"RecordedAccesses { ... }")
            .finish()
    }
}

impl<DB> RecordingDatabase<DB> {
    /// Create a new recording wrapper around an existing database.
    ///
    /// The `accesses` handle is shared — clone it before passing it here
    /// to retrieve recorded data after the database is consumed.
    pub fn new(inner: DB, accesses: RecordedAccesses) -> Self {
        Self { inner, accesses }
    }

    /// Consume the wrapper and return the inner database.
    pub fn into_inner(self) -> DB {
        self.inner
    }

    /// Get a reference to the inner database.
    pub fn inner(&self) -> &DB {
        &self.inner
    }

    /// Get a mutable reference to the inner database.
    pub fn inner_mut(&mut self) -> &mut DB {
        &mut self.inner
    }

    /// Get a reference to the shared recorded accesses handle.
    pub fn accesses(&self) -> &RecordedAccesses {
        &self.accesses
    }
}

impl<DB: Database<Error = ProviderError>> Database for RecordingDatabase<DB> {
    type Error = ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.accesses.record_account(address);
        self.inner.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.accesses.record_code_hash(code_hash);
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.accesses.record_storage(address, index);
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.accesses.record_block_hash(number);
        // EIP-2935: also record the corresponding storage slot in the history
        // contract so the proof generator includes it in the zone state witness.
        // This allows the prover's WitnessDatabase to serve block hashes from
        // the same MPT proofs used for regular storage, avoiding a separate
        // block hash witness field.
        self.accesses.record_storage(
            alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
            U256::from(number % alloy_eips::eip2935::HISTORY_SERVE_WINDOW as u64),
        );
        self.inner.block_hash(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple in-memory database for testing.
    struct TestDb;

    impl Database for TestDb {
        type Error = ProviderError;

        fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }

        fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Ok(U256::ZERO)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    #[test]
    fn test_recording_db_captures_accesses() {
        let accesses = RecordedAccesses::new();
        let mut db = RecordingDatabase::new(TestDb, accesses.clone());

        let addr1 = Address::with_last_byte(1);
        let addr2 = Address::with_last_byte(2);

        // Access accounts
        let _ = db.basic(addr1);
        let _ = db.basic(addr2);
        let _ = db.basic(addr1); // duplicate

        // Access storage
        let _ = db.storage(addr1, U256::from(10));
        let _ = db.storage(addr1, U256::from(20));
        let _ = db.storage(addr2, U256::from(30));

        // Use the cloned handle to retrieve accesses.
        assert_eq!(accesses.accessed_accounts().len(), 2);
        assert!(accesses.accessed_accounts().contains(&addr1));
        assert!(accesses.accessed_accounts().contains(&addr2));

        let storage = accesses.accessed_storage();
        assert_eq!(storage.len(), 2);
        assert_eq!(storage[&addr1].len(), 2);
        assert_eq!(storage[&addr2].len(), 1);
    }

    #[test]
    fn test_shared_handle_after_consume() {
        let accesses = RecordedAccesses::new();
        let mut db = RecordingDatabase::new(TestDb, accesses.clone());

        let addr = Address::with_last_byte(1);
        let _ = db.basic(addr);
        let _ = db.storage(addr, U256::from(42));

        // Consume the database.
        let _inner = db.into_inner();

        // The shared handle still has the recorded accesses.
        assert_eq!(accesses.accessed_accounts().len(), 1);
        assert!(accesses.accessed_accounts().contains(&addr));
        assert_eq!(accesses.accessed_storage()[&addr].len(), 1);
    }
}
