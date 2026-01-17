//! L2 Database for Privacy Zone.
//!
//! Implements reth_revm::Database for EVM execution.
//! Based on reth-exex-examples/rollup pattern but uses reth's ProviderFactory.

use alloy_primitives::{Address, B256, U256};
use reth_primitives::{Block, RecoveredBlock};
use reth_provider::OriginalValuesKnown;
use reth_revm::{
    db::{states::PlainStorageChangeset, BundleState, DBErrorMarker},
    state::{AccountInfo, Bytecode},
    Database as RevmDatabase,
};
use std::{
    collections::HashMap,
    error::Error,
    fmt::{Debug, Display, Formatter},
};

/// L2 Database error type.
#[derive(Debug)]
pub struct L2DbError(pub eyre::Error);

impl Display for L2DbError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Error for L2DbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

impl From<eyre::Error> for L2DbError {
    fn from(value: eyre::Report) -> Self {
        Self(value)
    }
}

impl DBErrorMarker for L2DbError {}

/// In-memory L2 database for EVM execution.
///
/// This is a simple implementation for development. Production should use
/// reth's ProviderFactory for persistent storage.
#[derive(Debug)]
pub struct L2Database {
    /// Account states: address -> AccountInfo
    accounts: HashMap<Address, AccountInfo>,
    /// Storage: address -> (slot -> value)
    storage: HashMap<Address, HashMap<U256, U256>>,
    /// Bytecode: code_hash -> bytecode
    bytecode: HashMap<B256, Bytecode>,
    /// Blocks: number -> block
    blocks: HashMap<u64, RecoveredBlock<Block>>,
    /// Current block number
    current_block: u64,
}

impl Default for L2Database {
    fn default() -> Self {
        Self::new_in_memory().expect("failed to create in-memory database")
    }
}

impl L2Database {
    /// Create a new in-memory database.
    pub fn new_in_memory() -> eyre::Result<Self> {
        Ok(Self {
            accounts: HashMap::new(),
            storage: HashMap::new(),
            bytecode: HashMap::new(),
            blocks: HashMap::new(),
            current_block: 0,
        })
    }

    /// Get account info by address.
    pub fn get_account(&self, address: Address) -> eyre::Result<Option<AccountInfo>> {
        Ok(self.accounts.get(&address).cloned())
    }

    /// Set account info.
    pub fn set_account(&mut self, address: Address, info: AccountInfo) {
        self.accounts.insert(address, info);
    }

    /// Credit balance to an address (for deposits).
    pub fn credit_balance(&mut self, address: Address, amount: U256) -> eyre::Result<()> {
        let account = self.accounts.entry(address).or_insert_with(AccountInfo::default);
        account.balance = account
            .balance
            .checked_add(amount)
            .ok_or_else(|| eyre::eyre!("balance overflow"))?;
        Ok(())
    }

    /// Get storage value.
    pub fn get_storage(&self, address: Address, slot: U256) -> eyre::Result<Option<U256>> {
        Ok(self
            .storage
            .get(&address)
            .and_then(|slots| slots.get(&slot))
            .copied())
    }

    /// Set storage value.
    pub fn set_storage(&mut self, address: Address, slot: U256, value: U256) {
        self.storage
            .entry(address)
            .or_insert_with(HashMap::new)
            .insert(slot, value);
    }

    /// Get block by number.
    pub fn get_block(&self, number: u64) -> eyre::Result<Option<RecoveredBlock<Block>>> {
        Ok(self.blocks.get(&number).cloned())
    }

    /// Insert block with bundle state.
    pub fn insert_block_with_bundle(
        &mut self,
        block: &RecoveredBlock<Block>,
        bundle: BundleState,
    ) -> eyre::Result<()> {
        let block_number = block.header().number;

        // Store block
        self.blocks.insert(block_number, block.clone());
        self.current_block = block_number;

        // Apply state changes from bundle
        let (changeset, _reverts) = bundle.to_plain_state_and_reverts(OriginalValuesKnown::Yes);

        // Apply account changes
        for (address, account) in changeset.accounts {
            if let Some(account) = account {
                self.accounts.insert(address, account);
            } else {
                self.accounts.remove(&address);
            }
        }

        // Apply storage changes
        for PlainStorageChangeset {
            address,
            wipe_storage,
            storage,
        } in changeset.storage
        {
            if wipe_storage {
                self.storage.remove(&address);
            }

            let account_storage = self.storage.entry(address).or_insert_with(HashMap::new);
            for (slot, value) in storage {
                if value.is_zero() {
                    account_storage.remove(&slot);
                } else {
                    account_storage.insert(slot, value);
                }
            }
        }

        // Store bytecode
        for (hash, bytecode) in changeset.contracts {
            self.bytecode.insert(hash, bytecode);
        }

        Ok(())
    }

    /// Get the current block number.
    pub fn current_block_number(&self) -> u64 {
        self.current_block
    }
}

impl RevmDatabase for L2Database {
    type Error = L2DbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.accounts.get(&address).cloned())
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self
            .bytecode
            .get(&code_hash)
            .cloned()
            .unwrap_or_default())
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self
            .storage
            .get(&address)
            .and_then(|slots| slots.get(&index))
            .copied()
            .unwrap_or_default())
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self
            .blocks
            .get(&number)
            .map(|b| b.hash())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_account_operations() {
        let mut db = L2Database::new_in_memory().unwrap();
        let addr = address!("1111111111111111111111111111111111111111");

        assert!(db.get_account(addr).unwrap().is_none());

        db.set_account(
            addr,
            AccountInfo {
                balance: U256::from(1000),
                nonce: 1,
                ..Default::default()
            },
        );

        let account = db.get_account(addr).unwrap().unwrap();
        assert_eq!(account.balance, U256::from(1000));
        assert_eq!(account.nonce, 1);
    }

    #[test]
    fn test_credit_balance() {
        let mut db = L2Database::new_in_memory().unwrap();
        let addr = address!("1111111111111111111111111111111111111111");

        db.credit_balance(addr, U256::from(100)).unwrap();
        let account = db.get_account(addr).unwrap().unwrap();
        assert_eq!(account.balance, U256::from(100));

        db.credit_balance(addr, U256::from(50)).unwrap();
        let account = db.get_account(addr).unwrap().unwrap();
        assert_eq!(account.balance, U256::from(150));
    }

    #[test]
    fn test_storage_operations() {
        let mut db = L2Database::new_in_memory().unwrap();
        let addr = address!("1111111111111111111111111111111111111111");
        let slot = U256::from(42);

        assert!(db.get_storage(addr, slot).unwrap().is_none());

        db.set_storage(addr, slot, U256::from(999));
        assert_eq!(db.get_storage(addr, slot).unwrap(), Some(U256::from(999)));
    }

    #[test]
    fn test_revm_database_trait() {
        use reth_revm::Database;

        let mut db = L2Database::new_in_memory().unwrap();
        let addr = address!("1111111111111111111111111111111111111111");

        db.set_account(
            addr,
            AccountInfo {
                balance: U256::from(5000),
                ..Default::default()
            },
        );

        let info = Database::basic(&mut db, addr).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(5000));
    }
}
