//! In-memory zone state for Privacy Zone.
//!
//! Simple state management using revm's CacheDB for EVM storage,
//! plus plain structs for zone-specific tracking (deposits, exits, cursors).
//! Can be swapped to reth's ProviderFactory later.

use crate::types::{Deposit, ExitIntent, L1Cursor, PendingTx, PzConfig, PzState};
use alloy_primitives::{Address, B256, U256};
use reth_revm::{
    Database as RevmDatabase,
    db::{CacheDB, EmptyDB},
    state::{AccountInfo, Bytecode},
};
use std::collections::VecDeque;

/// In-memory zone state.
///
/// Uses CacheDB for EVM state (accounts, storage, bytecode) and
/// simple collections for zone-specific data (deposits, exits).
#[derive(Debug)]
pub struct ZoneState {
    /// EVM state (accounts, storage, bytecode).
    db: CacheDB<EmptyDB>,
    /// Zone configuration.
    config: Option<PzConfig>,
    /// Current zone state (cursors, hashes, block height).
    state: PzState,
    /// Pending transactions to be included in next block.
    /// Deposits are at the front (forced inclusion), user txs follow.
    pending_txs: VecDeque<PendingTx>,
    /// Pending exits (not yet included in a batch).
    pending_exits: Vec<(B256, ExitIntent)>,
}

impl Default for ZoneState {
    fn default() -> Self {
        Self::new()
    }
}

impl ZoneState {
    /// Create a new empty zone state.
    pub fn new() -> Self {
        Self {
            db: CacheDB::new(EmptyDB::default()),
            config: None,
            state: PzState::default(),
            pending_txs: VecDeque::new(),
            pending_exits: Vec::new(),
        }
    }

    // ========================================================================
    // EVM State Access
    // ========================================================================

    /// Get account info.
    pub fn get_account(&self, address: Address) -> Option<&AccountInfo> {
        self.db.cache.accounts.get(&address).map(|a| &a.info)
    }

    /// Set account info.
    pub fn set_account(&mut self, address: Address, info: AccountInfo) {
        self.db.insert_account_info(address, info);
    }

    /// Update account with a closure.
    pub fn update_account<F>(&mut self, address: Address, f: F) -> eyre::Result<()>
    where
        F: FnOnce(Option<AccountInfo>) -> eyre::Result<AccountInfo>,
    {
        let current = self.get_account(address).cloned();
        let new_info = f(current)?;
        self.set_account(address, new_info);
        Ok(())
    }

    /// Get storage value.
    pub fn get_storage(&self, address: Address, slot: U256) -> U256 {
        self.db
            .cache
            .accounts
            .get(&address)
            .and_then(|a| a.storage.get(&slot))
            .copied()
            .unwrap_or_default()
    }

    /// Set storage value.
    pub fn set_storage(&mut self, address: Address, slot: U256, value: U256) {
        // Ensure account exists
        if !self.db.cache.accounts.contains_key(&address) {
            self.db.insert_account_info(address, AccountInfo::default());
        }
        if let Some(account) = self.db.cache.accounts.get_mut(&address) {
            account.storage.insert(slot, value);
        }
    }

    /// Get bytecode by hash.
    pub fn get_bytecode(&self, hash: B256) -> Option<&Bytecode> {
        self.db.cache.contracts.get(&hash)
    }

    /// Insert bytecode.
    pub fn insert_bytecode(&mut self, bytecode: Bytecode) {
        let hash = bytecode.hash_slow();
        self.db.cache.contracts.insert(hash, bytecode);
    }

    /// Get mutable reference to inner CacheDB for EVM execution.
    pub fn db_mut(&mut self) -> &mut CacheDB<EmptyDB> {
        &mut self.db
    }

    /// Get reference to inner CacheDB.
    pub fn db(&self) -> &CacheDB<EmptyDB> {
        &self.db
    }

    // ========================================================================
    // Zone Config & State
    // ========================================================================

    /// Get zone config.
    pub fn config(&self) -> Option<&PzConfig> {
        self.config.as_ref()
    }

    /// Set zone config.
    pub fn set_config(&mut self, config: PzConfig) {
        self.config = Some(config);
    }

    /// Get current zone state.
    pub fn zone_state(&self) -> &PzState {
        &self.state
    }

    /// Get mutable zone state.
    pub fn zone_state_mut(&mut self) -> &mut PzState {
        &mut self.state
    }

    /// Set zone state.
    pub fn set_zone_state(&mut self, state: PzState) {
        self.state = state;
    }

    // ========================================================================
    // Pending Transactions (Deposits + User Txs)
    // ========================================================================

    /// Queue a deposit as a pending transaction.
    pub fn queue_deposit(&mut self, cursor: L1Cursor, deposit: Deposit, hash: B256) {
        self.pending_txs
            .push_back(PendingTx::deposit(cursor, hash, deposit));
    }

    /// Get all pending transactions.
    pub fn pending_txs(&self) -> &VecDeque<PendingTx> {
        &self.pending_txs
    }

    /// Take all pending transactions for block building.
    pub fn take_pending_txs(&mut self) -> VecDeque<PendingTx> {
        std::mem::take(&mut self.pending_txs)
    }

    /// Take up to `limit` pending transactions for block building.
    pub fn take_pending_txs_limit(&mut self, limit: usize) -> Vec<PendingTx> {
        let count = limit.min(self.pending_txs.len());
        self.pending_txs.drain(..count).collect()
    }

    /// Remove deposits after a cursor (for reorgs).
    /// Only affects deposit transactions, not user txs.
    pub fn remove_deposits_after(&mut self, cursor: L1Cursor) {
        self.pending_txs.retain(|tx| {
            match tx {
                PendingTx::Deposit {
                    cursor: tx_cursor, ..
                } => !tx_cursor.is_after(&cursor),
                PendingTx::UserTx { .. } => true, // Keep user txs
            }
        });
    }

    /// Get count of pending deposits.
    pub fn pending_deposit_count(&self) -> usize {
        self.pending_txs.iter().filter(|tx| tx.is_deposit()).count()
    }

    // ========================================================================
    // Exits
    // ========================================================================

    /// Queue an exit.
    pub fn queue_exit(&mut self, exit: ExitIntent, hash: B256) {
        self.pending_exits.push((hash, exit));
    }

    /// Get pending exits.
    pub fn pending_exits(&self) -> &[(B256, ExitIntent)] {
        &self.pending_exits
    }

    /// Take pending exits for batching.
    pub fn take_pending_exits(&mut self) -> Vec<(B256, ExitIntent)> {
        std::mem::take(&mut self.pending_exits)
    }

    /// Remove exits after a zone block (for reorgs).
    pub fn remove_exits_after(&mut self, zone_block: u64) {
        self.pending_exits
            .retain(|(_, e)| e.zone_block <= zone_block);
    }
}

// Implement reth_revm::Database for ZoneState so it can be used directly with EVM
impl RevmDatabase for ZoneState {
    type Error = core::convert::Infallible;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.get_account(address).cloned())
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.get_bytecode(code_hash).cloned().unwrap_or_default())
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.get_storage(address, index))
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        // Zone doesn't need L1 block hashes via this interface
        // L1 block info comes through deposits
        Ok(B256::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_account_operations() {
        let mut state = ZoneState::new();
        let addr = address!("1111111111111111111111111111111111111111");

        assert!(state.get_account(addr).is_none());

        state.set_account(
            addr,
            AccountInfo {
                balance: U256::from(1000),
                nonce: 1,
                ..Default::default()
            },
        );

        let acc = state.get_account(addr).unwrap();
        assert_eq!(acc.balance, U256::from(1000));
        assert_eq!(acc.nonce, 1);
    }

    #[test]
    fn test_storage_operations() {
        let mut state = ZoneState::new();
        let addr = address!("1111111111111111111111111111111111111111");
        let slot = U256::from(42);

        assert_eq!(state.get_storage(addr, slot), U256::ZERO);

        state.set_storage(addr, slot, U256::from(999));
        assert_eq!(state.get_storage(addr, slot), U256::from(999));
    }

    #[test]
    fn test_deposit_queue() {
        let mut state = ZoneState::new();

        let deposit = Deposit {
            l1_block_hash: B256::ZERO,
            l1_block_number: 100,
            l1_timestamp: 1000,
            sender: address!("1111111111111111111111111111111111111111"),
            to: address!("2222222222222222222222222222222222222222"),
            amount: U256::from(1000),
            gas_limit: 0,
            data: Default::default(),
        };

        state.queue_deposit(
            L1Cursor::new(100, 0),
            deposit.clone(),
            B256::repeat_byte(0x01),
        );
        state.queue_deposit(L1Cursor::new(100, 1), deposit, B256::repeat_byte(0x02));

        assert_eq!(state.pending_txs().len(), 2);
        assert_eq!(state.pending_deposit_count(), 2);

        // Remove after cursor
        state.remove_deposits_after(L1Cursor::new(100, 0));
        assert_eq!(state.pending_txs().len(), 1);
    }

    #[test]
    fn test_revm_database_trait() {
        let mut state = ZoneState::new();
        let addr = address!("1111111111111111111111111111111111111111");

        state.set_account(
            addr,
            AccountInfo {
                balance: U256::from(5000),
                ..Default::default()
            },
        );

        // Use via Database trait
        let info = RevmDatabase::basic(&mut state, addr).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(5000));
    }
}
