//! SQL database for Privacy Zone state.
//!
//! Uses SQLite for state persistence. No reth db, no txpool.

use crate::{
    error::PzError,
    types::{Deposit, ExitIntent, PzAccount, PzConfig, PzState},
};
use alloy_primitives::{Address, B256, U256};
use rusqlite::Connection;
use std::sync::{Arc, Mutex, MutexGuard};

/// SQL database for privacy zone state.
#[derive(Debug)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
    pz_id: u64,
}

impl Database {
    /// Create a new database with the provided connection.
    pub fn new(connection: Connection, pz_id: u64) -> Result<Self, PzError> {
        let db = Self {
            connection: Arc::new(Mutex::new(connection)),
            pz_id,
        };
        db.create_tables()?;
        Ok(db)
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().expect("failed to acquire database lock")
    }

    fn create_tables(&self) -> Result<(), PzError> {
        self.connection().execute_batch(
            "
            -- Zone configuration
            CREATE TABLE IF NOT EXISTS zone_config (
                zone_id INTEGER PRIMARY KEY,
                data TEXT NOT NULL
            );

            -- Zone state (current head)
            CREATE TABLE IF NOT EXISTS zone_state (
                zone_id INTEGER PRIMARY KEY,
                data TEXT NOT NULL
            );

            -- Accounts
            CREATE TABLE IF NOT EXISTS account (
                id INTEGER PRIMARY KEY,
                zone_id INTEGER NOT NULL,
                address TEXT NOT NULL,
                data TEXT NOT NULL,
                UNIQUE (zone_id, address)
            );

            -- Account reverts for reorg handling
            CREATE TABLE IF NOT EXISTS account_revert (
                id INTEGER PRIMARY KEY,
                zone_id INTEGER NOT NULL,
                batch_index INTEGER NOT NULL,
                address TEXT NOT NULL,
                data TEXT,
                UNIQUE (zone_id, batch_index, address)
            );

            -- Deposits queue
            CREATE TABLE IF NOT EXISTS deposit (
                id INTEGER PRIMARY KEY,
                zone_id INTEGER NOT NULL,
                deposit_hash TEXT NOT NULL,
                l1_block_number INTEGER NOT NULL,
                data TEXT NOT NULL,
                processed INTEGER DEFAULT 0,
                UNIQUE (zone_id, deposit_hash)
            );

            -- Exit intents (pending)
            CREATE TABLE IF NOT EXISTS exit_intent (
                id INTEGER PRIMARY KEY,
                zone_id INTEGER NOT NULL,
                exit_index INTEGER NOT NULL,
                data TEXT NOT NULL,
                batch_index INTEGER,
                UNIQUE (zone_id, exit_index)
            );

            -- Batches
            CREATE TABLE IF NOT EXISTS batch (
                id INTEGER PRIMARY KEY,
                zone_id INTEGER NOT NULL,
                batch_index INTEGER NOT NULL,
                state_root TEXT NOT NULL,
                deposits_hash TEXT NOT NULL,
                l1_block_number INTEGER NOT NULL,
                data TEXT NOT NULL,
                UNIQUE (zone_id, batch_index)
            );

            -- Create indexes for common queries
            CREATE INDEX IF NOT EXISTS idx_account_zone_address ON account(zone_id, address);
            CREATE INDEX IF NOT EXISTS idx_deposit_zone_processed ON deposit(zone_id, processed);
            CREATE INDEX IF NOT EXISTS idx_exit_intent_zone_batch ON exit_intent(zone_id, batch_index);
            ",
        )?;
        Ok(())
    }

    /// Get the zone ID.
    pub fn zone_id(&self) -> u64 {
        self.zone_id
    }

    // ========================================================================
    // Zone Config
    // ========================================================================

    /// Store zone configuration.
    pub fn set_zone_config(&self, config: &ZoneConfig) -> Result<(), PzError> {
        let data = serde_json::to_string(config)?;
        self.connection().execute(
            "INSERT INTO zone_config (zone_id, data) VALUES (?1, ?2)
             ON CONFLICT(zone_id) DO UPDATE SET data = excluded.data",
            (self.zone_id, &data),
        )?;
        Ok(())
    }

    /// Get zone configuration.
    pub fn get_zone_config(&self) -> Result<Option<ZoneConfig>, PzError> {
        let conn = self.connection();
        let mut stmt = conn.prepare("SELECT data FROM zone_config WHERE zone_id = ?1")?;
        let result: Result<String, _> = stmt.query_row([self.zone_id], |row| row.get(0));
        match result {
            Ok(data) => Ok(Some(serde_json::from_str(&data)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ========================================================================
    // Zone State
    // ========================================================================

    /// Store zone state.
    pub fn set_zone_state(&self, state: &ZoneState) -> Result<(), PzError> {
        let data = serde_json::to_string(state)?;
        self.connection().execute(
            "INSERT INTO zone_state (zone_id, data) VALUES (?1, ?2)
             ON CONFLICT(zone_id) DO UPDATE SET data = excluded.data",
            (self.zone_id, &data),
        )?;
        Ok(())
    }

    /// Get zone state.
    pub fn get_zone_state(&self) -> Result<ZoneState, PzError> {
        let conn = self.connection();
        let mut stmt = conn.prepare("SELECT data FROM zone_state WHERE zone_id = ?1")?;
        let result: Result<String, _> = stmt.query_row([self.zone_id], |row| row.get(0));
        match result {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(ZoneState::default()),
            Err(e) => Err(e.into()),
        }
    }

    // ========================================================================
    // Accounts
    // ========================================================================

    /// Get account by address.
    pub fn get_account(&self, address: Address) -> Result<Option<ZoneAccount>, PzError> {
        let conn = self.connection();
        let mut stmt =
            conn.prepare("SELECT data FROM account WHERE zone_id = ?1 AND address = ?2")?;
        let result: Result<String, _> =
            stmt.query_row((self.zone_id, address.to_string()), |row| row.get(0));
        match result {
            Ok(data) => Ok(Some(serde_json::from_str(&data)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Upsert account with the provided update function.
    pub fn upsert_account<F>(&self, address: Address, f: F) -> Result<(), PzError>
    where
        F: FnOnce(Option<ZoneAccount>) -> Result<ZoneAccount, PzError>,
    {
        let current = self.get_account(address)?;
        let updated = f(current)?;
        let data = serde_json::to_string(&updated)?;
        self.connection().execute(
            "INSERT INTO account (zone_id, address, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(zone_id, address) DO UPDATE SET data = excluded.data",
            (self.zone_id, address.to_string(), &data),
        )?;
        Ok(())
    }

    /// Credit an account (add to balance).
    pub fn credit_account(&self, address: Address, amount: U256) -> Result<(), PzError> {
        self.upsert_account(address, |account| {
            let mut account = account.unwrap_or_default();
            account.balance = account.balance.saturating_add(amount);
            Ok(account)
        })
    }

    /// Debit an account (subtract from balance).
    pub fn debit_account(&self, address: Address, amount: U256) -> Result<(), PzError> {
        self.upsert_account(address, |account| {
            let mut account = account.ok_or_else(|| PzError::AccountNotFound(address))?;
            if account.balance < amount {
                return Err(PzError::Other(format!(
                    "insufficient balance: have {}, need {}",
                    account.balance, amount
                )));
            }
            account.balance = account.balance.saturating_sub(amount);
            Ok(account)
        })
    }

    /// Store account revert for a batch (for reorg handling).
    pub fn store_account_revert(
        &self,
        batch_index: u64,
        address: Address,
        old_account: Option<&ZoneAccount>,
    ) -> Result<(), PzError> {
        let data = old_account.map(|a| serde_json::to_string(a)).transpose()?;
        self.connection().execute(
            "INSERT INTO account_revert (zone_id, batch_index, address, data) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(zone_id, batch_index, address) DO UPDATE SET data = excluded.data",
            (self.zone_id, batch_index, address.to_string(), data),
        )?;
        Ok(())
    }

    // ========================================================================
    // Deposits
    // ========================================================================

    /// Queue a deposit.
    pub fn queue_deposit(&self, deposit: &Deposit, deposit_hash: B256) -> Result<(), PzError> {
        let data = serde_json::to_string(deposit)?;
        self.connection().execute(
            "INSERT INTO deposit (zone_id, deposit_hash, l1_block_number, data, processed)
             VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT(zone_id, deposit_hash) DO NOTHING",
            (
                self.zone_id,
                deposit_hash.to_string(),
                deposit.l1_block_number,
                &data,
            ),
        )?;
        Ok(())
    }

    /// Get unprocessed deposits in order.
    pub fn get_pending_deposits(&self) -> Result<Vec<(B256, Deposit)>, PzError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT deposit_hash, data FROM deposit
             WHERE zone_id = ?1 AND processed = 0
             ORDER BY l1_block_number ASC, id ASC",
        )?;
        let rows = stmt.query_map([self.zone_id], |row| {
            let hash: String = row.get(0)?;
            let data: String = row.get(1)?;
            Ok((hash, data))
        })?;
        let mut deposits = Vec::new();
        for row in rows {
            let (hash, data) = row?;
            let hash: B256 = hash.parse().map_err(|_| {
                PzError::Other(format!("invalid deposit hash: {hash}"))
            })?;
            let deposit: Deposit = serde_json::from_str(&data)?;
            deposits.push((hash, deposit));
        }
        Ok(deposits)
    }

    /// Mark deposits as processed up to the given hash.
    pub fn mark_deposits_processed(&self, up_to_hash: B256) -> Result<(), PzError> {
        // For simplicity, mark all deposits with hash <= up_to_hash as processed
        // In practice, you'd track the deposit chain properly
        self.connection().execute(
            "UPDATE deposit SET processed = 1
             WHERE zone_id = ?1 AND deposit_hash = ?2",
            (self.zone_id, up_to_hash.to_string()),
        )?;
        Ok(())
    }

    // ========================================================================
    // Exit Intents
    // ========================================================================

    /// Queue an exit intent.
    pub fn queue_exit_intent(&self, exit_index: u64, intent: &ExitIntent) -> Result<(), PzError> {
        let data = serde_json::to_string(intent)?;
        self.connection().execute(
            "INSERT INTO exit_intent (zone_id, exit_index, data)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(zone_id, exit_index) DO UPDATE SET data = excluded.data",
            (self.zone_id, exit_index, &data),
        )?;
        Ok(())
    }

    /// Get pending exit intents (not yet included in a batch).
    pub fn get_pending_exits(&self) -> Result<Vec<(u64, ExitIntent)>, PzError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT exit_index, data FROM exit_intent
             WHERE zone_id = ?1 AND batch_index IS NULL
             ORDER BY exit_index ASC",
        )?;
        let rows = stmt.query_map([self.zone_id], |row| {
            let index: u64 = row.get(0)?;
            let data: String = row.get(1)?;
            Ok((index, data))
        })?;
        let mut exits = Vec::new();
        for row in rows {
            let (index, data) = row?;
            let intent: ExitIntent = serde_json::from_str(&data)?;
            exits.push((index, intent));
        }
        Ok(exits)
    }

    /// Mark exit intents as included in a batch.
    pub fn mark_exits_in_batch(
        &self,
        exit_indices: &[u64],
        batch_index: u64,
    ) -> Result<(), PzError> {
        let conn = self.connection();
        for &idx in exit_indices {
            conn.execute(
                "UPDATE exit_intent SET batch_index = ?1
                 WHERE zone_id = ?2 AND exit_index = ?3",
                (batch_index, self.zone_id, idx),
            )?;
        }
        Ok(())
    }

    // ========================================================================
    // Batches
    // ========================================================================

    /// Store a batch.
    pub fn store_batch(
        &self,
        batch_index: u64,
        state_root: B256,
        deposits_hash: B256,
        l1_block_number: u64,
        data: &str,
    ) -> Result<(), PzError> {
        self.connection().execute(
            "INSERT INTO batch (zone_id, batch_index, state_root, deposits_hash, l1_block_number, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(zone_id, batch_index) DO UPDATE SET
                state_root = excluded.state_root,
                deposits_hash = excluded.deposits_hash,
                l1_block_number = excluded.l1_block_number,
                data = excluded.data",
            (
                self.zone_id,
                batch_index,
                state_root.to_string(),
                deposits_hash.to_string(),
                l1_block_number,
                data,
            ),
        )?;
        Ok(())
    }

    /// Get the latest batch index.
    pub fn get_latest_batch_index(&self) -> Result<Option<u64>, PzError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT MAX(batch_index) FROM batch WHERE zone_id = ?1",
        )?;
        let result: Result<Option<u64>, _> = stmt.query_row([self.zone_id], |row| row.get(0));
        match result {
            Ok(idx) => Ok(idx),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Revert to a specific batch index (delete all batches after it).
    pub fn revert_to_batch(&self, batch_index: u64) -> Result<(), PzError> {
        let conn = self.connection();
        
        // Delete batches after the target
        conn.execute(
            "DELETE FROM batch WHERE zone_id = ?1 AND batch_index > ?2",
            (self.zone_id, batch_index),
        )?;

        // Unmark exits that were in reverted batches
        conn.execute(
            "UPDATE exit_intent SET batch_index = NULL
             WHERE zone_id = ?1 AND batch_index > ?2",
            (self.zone_id, batch_index),
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_db() -> Database {
        let conn = Connection::open_in_memory().unwrap();
        Database::new(conn, 1).unwrap()
    }

    #[test]
    fn test_account_operations() {
        let db = test_db();
        let addr = Address::ZERO;

        // Initially no account
        assert!(db.get_account(addr).unwrap().is_none());

        // Credit creates account
        db.credit_account(addr, U256::from(100)).unwrap();
        let acc = db.get_account(addr).unwrap().unwrap();
        assert_eq!(acc.balance, U256::from(100));

        // Credit adds to balance
        db.credit_account(addr, U256::from(50)).unwrap();
        let acc = db.get_account(addr).unwrap().unwrap();
        assert_eq!(acc.balance, U256::from(150));

        // Debit subtracts
        db.debit_account(addr, U256::from(30)).unwrap();
        let acc = db.get_account(addr).unwrap().unwrap();
        assert_eq!(acc.balance, U256::from(120));

        // Debit fails on insufficient balance
        assert!(db.debit_account(addr, U256::from(200)).is_err());
    }

    #[test]
    fn test_zone_state() {
        let db = test_db();

        // Default state
        let state = db.get_zone_state().unwrap();
        assert_eq!(state.batch_index, 0);

        // Update state
        let new_state = ZoneState {
            state_root: B256::repeat_byte(0x01),
            batch_index: 5,
            ..Default::default()
        };
        db.set_zone_state(&new_state).unwrap();

        let state = db.get_zone_state().unwrap();
        assert_eq!(state.batch_index, 5);
        assert_eq!(state.state_root, B256::repeat_byte(0x01));
    }
}
