//! Recording database wrapper for witness generation.
//!
//! Wraps any revm [`Database`] to record which accounts and storage slots are
//! accessed during EVM execution. After execution, the recorded accesses can be
//! used to generate MPT proofs for the [`ZoneStateWitness`].

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};
use reth_errors::ProviderError;
use reth_revm::Database;
use revm::state::{AccountInfo, Bytecode};

/// A database wrapper that records all state accesses.
///
/// Delegates all reads to the inner database but logs every `basic()`,
/// `storage()`, and `code_by_hash()` call. After execution, the recorded
/// accesses can be extracted via [`accessed_accounts`](Self::accessed_accounts)
/// and [`accessed_storage`](Self::accessed_storage).
pub struct RecordingDatabase<DB> {
    inner: DB,
    /// Accounts whose `basic()` was called.
    accounts: BTreeSet<Address>,
    /// Storage slots read per account.
    storage: BTreeMap<Address, BTreeSet<U256>>,
    /// Code hashes that were looked up.
    code_hashes: BTreeSet<B256>,
    /// Block hashes that were looked up.
    block_hashes: BTreeSet<u64>,
}

impl<DB> RecordingDatabase<DB> {
    /// Create a new recording wrapper around an existing database.
    pub fn new(inner: DB) -> Self {
        Self {
            inner,
            accounts: BTreeSet::new(),
            storage: BTreeMap::new(),
            code_hashes: BTreeSet::new(),
            block_hashes: BTreeSet::new(),
        }
    }

    /// Returns the set of all accessed account addresses.
    pub fn accessed_accounts(&self) -> &BTreeSet<Address> {
        &self.accounts
    }

    /// Returns the set of all accessed storage slots, grouped by account.
    pub fn accessed_storage(&self) -> &BTreeMap<Address, BTreeSet<U256>> {
        &self.storage
    }

    /// Returns the set of all accessed code hashes.
    pub fn accessed_code_hashes(&self) -> &BTreeSet<B256> {
        &self.code_hashes
    }

    /// Returns the set of all accessed block numbers (for BLOCKHASH).
    pub fn accessed_block_hashes(&self) -> &BTreeSet<u64> {
        &self.block_hashes
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
}

impl<DB: Database<Error = ProviderError>> Database for RecordingDatabase<DB> {
    type Error = ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.accounts.insert(address);
        self.inner.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.code_hashes.insert(code_hash);
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.accounts.insert(address);
        self.storage
            .entry(address)
            .or_default()
            .insert(index);
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.block_hashes.insert(number);
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
        let mut db = RecordingDatabase::new(TestDb);

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

        assert_eq!(db.accessed_accounts().len(), 2);
        assert!(db.accessed_accounts().contains(&addr1));
        assert!(db.accessed_accounts().contains(&addr2));

        assert_eq!(db.accessed_storage().len(), 2);
        assert_eq!(db.accessed_storage()[&addr1].len(), 2);
        assert_eq!(db.accessed_storage()[&addr2].len(), 1);
    }
}
