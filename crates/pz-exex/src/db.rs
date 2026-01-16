//! SQL database for Privacy Zone state with revm::Database implementation.
//!
//! Based on reth-exex-examples/rollup pattern with Signet-inspired improvements.

use crate::{
    error::PzDbError,
    types::{Deposit, ExitIntent, L1Cursor, PzConfig, PzState},
};
use alloy_primitives::{Address, Bytes, B256, U256};
use reth_primitives_traits::StorageEntry;
use reth_provider::OriginalValuesKnown;
use reth_revm::{
    db::{
        states::{reverts::RevertToSlot, PlainStorageChangeset, PlainStorageRevert},
        BundleState,
    },
    state::{AccountInfo, Bytecode},
};
use rusqlite::Connection;
use std::{
    collections::{hash_map::Entry, HashMap},
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard},
};

/// Type used to initialize revm's bundle state.
type BundleStateInit =
    HashMap<Address, (Option<AccountInfo>, Option<AccountInfo>, HashMap<B256, (U256, U256)>)>;

/// Types used inside RevertsInit.
pub type AccountRevertInit = (Option<Option<AccountInfo>>, Vec<StorageEntry>);

/// Type used to initialize revm's reverts.
pub type RevertsInit = HashMap<Address, AccountRevertInit>;

/// SQL database for privacy zone state.
#[derive(Debug)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
}

impl Database {
    /// Create new database with the provided connection.
    pub fn new(connection: Connection) -> eyre::Result<Self> {
        let database = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        database.create_tables()?;
        Ok(database)
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().expect("failed to acquire database lock")
    }

    fn create_tables(&self) -> eyre::Result<()> {
        self.connection().execute_batch(
            "CREATE TABLE IF NOT EXISTS block (
                id     INTEGER PRIMARY KEY,
                number TEXT UNIQUE,
                data   TEXT
            );
            CREATE TABLE IF NOT EXISTS account (
                id      INTEGER PRIMARY KEY,
                address TEXT UNIQUE,
                data    TEXT
            );
            CREATE TABLE IF NOT EXISTS account_revert (
                id           INTEGER PRIMARY KEY,
                block_number TEXT,
                address      TEXT,
                data         TEXT,
                UNIQUE (block_number, address)
            );
            CREATE TABLE IF NOT EXISTS storage (
                id      INTEGER PRIMARY KEY,
                address TEXT,
                key     TEXT,
                data    TEXT,
                UNIQUE (address, key)
            );
            CREATE TABLE IF NOT EXISTS storage_revert (
                id           INTEGER PRIMARY KEY,
                block_number TEXT,
                address      TEXT,
                key          TEXT,
                data         TEXT,
                UNIQUE (block_number, address, key)
            );
            CREATE TABLE IF NOT EXISTS bytecode (
                id   INTEGER PRIMARY KEY,
                hash TEXT UNIQUE,
                data TEXT
            );
            -- Zone configuration (singleton)
            CREATE TABLE IF NOT EXISTS zone_config (
                id   INTEGER PRIMARY KEY CHECK (id = 1),
                data TEXT NOT NULL
            );
            -- Zone state (singleton, current head state)
            CREATE TABLE IF NOT EXISTS zone_state (
                id   INTEGER PRIMARY KEY CHECK (id = 1),
                data TEXT NOT NULL
            );
            -- Deposits from L1 (for revert tracking)
            CREATE TABLE IF NOT EXISTS deposit (
                id              INTEGER PRIMARY KEY,
                l1_block        INTEGER NOT NULL,
                log_index       INTEGER NOT NULL,
                deposit_hash    TEXT NOT NULL,
                data            TEXT NOT NULL,
                zone_block      INTEGER,
                UNIQUE (l1_block, log_index)
            );
            -- Zone block journal hashes (for provenance)
            CREATE TABLE IF NOT EXISTS journal (
                zone_block      INTEGER PRIMARY KEY,
                journal_hash    TEXT NOT NULL,
                l1_block        INTEGER NOT NULL,
                log_index       INTEGER NOT NULL
            );
            -- Exit intents (withdrawals from zone to L1)
            CREATE TABLE IF NOT EXISTS exit_intent (
                id              INTEGER PRIMARY KEY,
                zone_block      INTEGER NOT NULL,
                exit_index      INTEGER NOT NULL,
                exit_hash       TEXT NOT NULL,
                data            TEXT NOT NULL,
                batch_index     INTEGER,
                UNIQUE (zone_block, exit_index)
            );
            -- Create index for deposit lookups
            CREATE INDEX IF NOT EXISTS idx_deposit_zone_block ON deposit(zone_block);
            -- Create index for exit lookups
            CREATE INDEX IF NOT EXISTS idx_exit_batch ON exit_intent(batch_index);",
        )?;
        Ok(())
    }

    /// Insert block with bundle into the database.
    pub fn insert_block_with_bundle(
        &self,
        block_number: u64,
        bundle: BundleState,
    ) -> eyre::Result<()> {
        let mut connection = self.connection();
        let tx = connection.transaction()?;

        tx.execute(
            "INSERT INTO block (number, data) VALUES (?, ?)",
            (block_number.to_string(), "{}"),
        )?;

        let (changeset, reverts) = bundle.to_plain_state_and_reverts(OriginalValuesKnown::Yes);

        for (address, account) in changeset.accounts {
            if let Some(account) = account {
                tx.execute(
                    "INSERT INTO account (address, data) VALUES (?, ?) ON CONFLICT(address) DO UPDATE SET data = excluded.data",
                    (address.to_string(), serde_json::to_string(&account)?),
                )?;
            } else {
                tx.execute(
                    "DELETE FROM account WHERE address = ?",
                    (address.to_string(),),
                )?;
            }
        }

        if reverts.accounts.len() > 1 {
            eyre::bail!("too many blocks in account reverts");
        }
        if let Some(account_reverts) = reverts.accounts.into_iter().next() {
            for (address, account) in account_reverts {
                tx.execute(
                    "INSERT INTO account_revert (block_number, address, data) VALUES (?, ?, ?) ON CONFLICT(block_number, address) DO UPDATE SET data = excluded.data",
                    (block_number.to_string(), address.to_string(), serde_json::to_string(&account)?),
                )?;
            }
        }

        for PlainStorageChangeset { address, wipe_storage, storage } in changeset.storage {
            if wipe_storage {
                tx.execute(
                    "DELETE FROM storage WHERE address = ?",
                    (address.to_string(),),
                )?;
            }

            for (key, data) in storage {
                tx.execute(
                    "INSERT INTO storage (address, key, data) VALUES (?, ?, ?) ON CONFLICT(address, key) DO UPDATE SET data = excluded.data",
                    (address.to_string(), B256::from(key).to_string(), data.to_string()),
                )?;
            }
        }

        if reverts.storage.len() > 1 {
            eyre::bail!("too many blocks in storage reverts");
        }
        if let Some(storage_reverts) = reverts.storage.into_iter().next() {
            for PlainStorageRevert { address, wiped, storage_revert } in storage_reverts {
                let storage = storage_revert
                    .into_iter()
                    .map(|(k, v)| (B256::new(k.to_be_bytes()), v))
                    .collect::<Vec<_>>();
                let wiped_storage = if wiped {
                    get_storages(&tx, address)?
                        .into_iter()
                        .map(|(k, v)| (k, RevertToSlot::Some(v)))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                for (key, data) in storage.into_iter().chain(wiped_storage) {
                    let data_str = match data {
                        RevertToSlot::Some(value) => value.to_string(),
                        RevertToSlot::Destroyed => "destroyed".to_string(),
                    };
                    tx.execute(
                        "INSERT INTO storage_revert (block_number, address, key, data) VALUES (?, ?, ?, ?) ON CONFLICT(block_number, address, key) DO UPDATE SET data = excluded.data",
                        (block_number.to_string(), address.to_string(), key.to_string(), data_str),
                    )?;
                }
            }
        }

        for (hash, bytecode) in changeset.contracts {
            tx.execute(
                "INSERT INTO bytecode (hash, data) VALUES (?, ?) ON CONFLICT(hash) DO NOTHING",
                (hash.to_string(), bytecode.bytecode().to_string()),
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Reverts the tip block from the database.
    pub fn revert_tip_block(&self, block_number: u64) -> eyre::Result<()> {
        let mut connection = self.connection();
        let tx = connection.transaction()?;

        let tip_block_number = tx
            .query_row::<String, _, _>(
                "SELECT number FROM block ORDER BY number DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .map(|data| u64::from_str(&data))??;

        if block_number != tip_block_number {
            eyre::bail!(
                "Reverts can only be done from the tip. Attempted to revert block {} with tip block {}",
                block_number,
                tip_block_number
            );
        }

        tx.execute("DELETE FROM block WHERE number = ?", (block_number.to_string(),))?;

        let mut state = BundleStateInit::new();
        let mut reverts = RevertsInit::new();

        let account_reverts = tx
            .prepare("SELECT address, data FROM account_revert WHERE block_number = ?")?
            .query((block_number.to_string(),))?
            .mapped(|row| {
                Ok((
                    Address::from_str(row.get_ref(0)?.as_str()?),
                    serde_json::from_str::<Option<AccountInfo>>(row.get_ref(1)?.as_str()?),
                ))
            })
            .map(|result| {
                let (address, data) = result?;
                Ok((address?, data?))
            })
            .collect::<eyre::Result<Vec<_>>>()?;

        for (address, old_info) in account_reverts {
            reverts.entry(address).or_default().0 = Some(old_info.clone());

            match state.entry(address) {
                Entry::Vacant(entry) => {
                    let new_info = get_account(&tx, address)?;
                    entry.insert((old_info, new_info, HashMap::new()));
                }
                Entry::Occupied(mut entry) => {
                    entry.get_mut().0 = old_info;
                }
            }
        }

        let storage_reverts = tx
            .prepare("SELECT address, key, data FROM storage_revert WHERE block_number = ?")?
            .query((block_number.to_string(),))?
            .mapped(|row| {
                Ok((
                    row.get_ref(0)?.as_str()?.to_string(),
                    row.get_ref(1)?.as_str()?.to_string(),
                    row.get_ref(2)?.as_str()?.to_string(),
                ))
            })
            .map(|result| {
                let (address_str, key_str, data_str) = result?;
                let data = if data_str == "destroyed" { U256::ZERO } else { U256::from_str(&data_str)? };
                Ok((Address::from_str(&address_str)?, B256::from_str(&key_str)?, data))
            })
            .collect::<eyre::Result<Vec<_>>>()?;

        for (address, key, old_data) in storage_reverts.into_iter().rev() {
            let old_storage = StorageEntry { key, value: old_data };
            reverts.entry(address).or_default().1.push(old_storage);

            let account_state = match state.entry(address) {
                Entry::Vacant(entry) => {
                    let present_info = get_account(&tx, address)?;
                    entry.insert((present_info.clone(), present_info, HashMap::new()))
                }
                Entry::Occupied(entry) => entry.into_mut(),
            };

            match account_state.2.entry(old_storage.key) {
                Entry::Vacant(entry) => {
                    let new_value = get_storage(&tx, address, old_storage.key)?.unwrap_or_default();
                    entry.insert((old_storage.value, new_value));
                }
                Entry::Occupied(mut entry) => {
                    entry.get_mut().0 = old_storage.value;
                }
            };
        }

        for (address, (old_account, new_account, storage)) in state {
            if old_account != new_account {
                if let Some(account) = old_account {
                    upsert_account(&tx, address, |_| Ok(account))?;
                } else {
                    delete_account(&tx, address)?;
                }
            }

            for (storage_key, (old_storage_value, _new_storage_value)) in storage {
                delete_storage(&tx, address, storage_key)?;
                if !old_storage_value.is_zero() {
                    upsert_storage(&tx, address, storage_key, old_storage_value)?;
                }
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Get the latest block number.
    pub fn get_latest_block_number(&self) -> eyre::Result<Option<u64>> {
        let result = self.connection().query_row::<String, _, _>(
            "SELECT number FROM block ORDER BY number DESC LIMIT 1",
            [],
            |row| row.get(0),
        );
        match result {
            Ok(data) => Ok(Some(u64::from_str(&data)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ========================================================================
    // Zone Config & State (Signet-inspired patterns)
    // ========================================================================

    /// Store zone configuration.
    pub fn set_zone_config(&self, config: &PzConfig) -> eyre::Result<()> {
        let data = serde_json::to_string(config)?;
        self.connection().execute(
            "INSERT INTO zone_config (id, data) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            [&data],
        )?;
        Ok(())
    }

    /// Get zone configuration.
    pub fn get_zone_config(&self) -> eyre::Result<Option<PzConfig>> {
        match self.connection().query_row::<String, _, _>(
            "SELECT data FROM zone_config WHERE id = 1",
            [],
            |row| row.get(0),
        ) {
            Ok(data) => Ok(Some(serde_json::from_str(&data)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Store zone state.
    pub fn set_zone_state(&self, state: &PzState) -> eyre::Result<()> {
        let data = serde_json::to_string(state)?;
        self.connection().execute(
            "INSERT INTO zone_state (id, data) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            [&data],
        )?;
        Ok(())
    }

    /// Get zone state.
    pub fn get_zone_state(&self) -> eyre::Result<PzState> {
        match self.connection().query_row::<String, _, _>(
            "SELECT data FROM zone_state WHERE id = 1",
            [],
            |row| row.get(0),
        ) {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(PzState::default()),
            Err(e) => Err(e.into()),
        }
    }

    // ========================================================================
    // Deposits
    // ========================================================================

    /// Queue a deposit from L1.
    pub fn queue_deposit(
        &self,
        cursor: L1Cursor,
        deposit: &Deposit,
        deposit_hash: B256,
    ) -> eyre::Result<()> {
        let data = serde_json::to_string(deposit)?;
        self.connection().execute(
            "INSERT INTO deposit (l1_block, log_index, deposit_hash, data)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(l1_block, log_index) DO NOTHING",
            (
                cursor.block_number,
                cursor.log_index,
                deposit_hash.to_string(),
                &data,
            ),
        )?;
        Ok(())
    }

    /// Get pending deposits (not yet processed into a zone block).
    pub fn get_pending_deposits(&self) -> eyre::Result<Vec<(L1Cursor, B256, Deposit)>> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT l1_block, log_index, deposit_hash, data FROM deposit
             WHERE zone_block IS NULL
             ORDER BY l1_block ASC, log_index ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let l1_block: u64 = row.get(0)?;
            let log_index: u64 = row.get(1)?;
            let hash: String = row.get(2)?;
            let data: String = row.get(3)?;
            Ok((l1_block, log_index, hash, data))
        })?;
        let mut deposits = Vec::new();
        for row in rows {
            let (l1_block, log_index, hash, data) = row?;
            let cursor = L1Cursor::new(l1_block, log_index);
            let hash: B256 = hash.parse().map_err(|_| eyre::eyre!("invalid hash"))?;
            let deposit: Deposit = serde_json::from_str(&data)?;
            deposits.push((cursor, hash, deposit));
        }
        Ok(deposits)
    }

    /// Mark deposits as processed in a zone block.
    pub fn mark_deposits_processed(&self, zone_block: u64, up_to: L1Cursor) -> eyre::Result<()> {
        self.connection().execute(
            "UPDATE deposit SET zone_block = ?1
             WHERE zone_block IS NULL AND (l1_block < ?2 OR (l1_block = ?2 AND log_index <= ?3))",
            (zone_block, up_to.block_number, up_to.log_index),
        )?;
        Ok(())
    }

    /// Unmark deposits processed after a zone block (for revert).
    pub fn unmark_deposits_after(&self, zone_block: u64) -> eyre::Result<()> {
        self.connection().execute(
            "UPDATE deposit SET zone_block = NULL WHERE zone_block > ?1",
            [zone_block],
        )?;
        Ok(())
    }

    /// Remove deposits after a cursor (for L1 reorg).
    pub fn remove_deposits_after(&self, cursor: L1Cursor) -> eyre::Result<()> {
        self.connection().execute(
            "DELETE FROM deposit
             WHERE l1_block > ?1 OR (l1_block = ?1 AND log_index > ?2)",
            (cursor.block_number, cursor.log_index),
        )?;
        Ok(())
    }

    // ========================================================================
    // Journal (provenance tracking like Signet)
    // ========================================================================

    /// Store a journal entry for a zone block.
    pub fn insert_journal(
        &self,
        zone_block: u64,
        journal_hash: B256,
        cursor: L1Cursor,
    ) -> eyre::Result<()> {
        self.connection().execute(
            "INSERT INTO journal (zone_block, journal_hash, l1_block, log_index)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(zone_block) DO UPDATE SET
                journal_hash = excluded.journal_hash,
                l1_block = excluded.l1_block,
                log_index = excluded.log_index",
            (
                zone_block,
                journal_hash.to_string(),
                cursor.block_number,
                cursor.log_index,
            ),
        )?;
        Ok(())
    }

    /// Get the journal hash for a zone block.
    pub fn get_journal_hash(&self, zone_block: u64) -> eyre::Result<Option<B256>> {
        match self.connection().query_row::<String, _, _>(
            "SELECT journal_hash FROM journal WHERE zone_block = ?1",
            [zone_block],
            |row| row.get(0),
        ) {
            Ok(data) => Ok(Some(data.parse().map_err(|_| eyre::eyre!("invalid hash"))?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Remove journal entries after a zone block (for revert).
    pub fn remove_journals_after(&self, zone_block: u64) -> eyre::Result<()> {
        self.connection().execute(
            "DELETE FROM journal WHERE zone_block > ?1",
            [zone_block],
        )?;
        Ok(())
    }

    // ========================================================================
    // Exit Intents
    // ========================================================================

    /// Record an exit intent.
    pub fn insert_exit(&self, exit: &ExitIntent, exit_hash: B256) -> eyre::Result<()> {
        let data = serde_json::to_string(exit)?;
        self.connection().execute(
            "INSERT INTO exit_intent (zone_block, exit_index, exit_hash, data)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(zone_block, exit_index) DO NOTHING",
            (exit.zone_block, exit.exit_index, exit_hash.to_string(), &data),
        )?;
        Ok(())
    }

    /// Get pending exits (not yet included in a batch).
    pub fn get_pending_exits(&self) -> eyre::Result<Vec<(B256, ExitIntent)>> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT exit_hash, data FROM exit_intent
             WHERE batch_index IS NULL
             ORDER BY zone_block ASC, exit_index ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let hash: String = row.get(0)?;
            let data: String = row.get(1)?;
            Ok((hash, data))
        })?;
        let mut exits = Vec::new();
        for row in rows {
            let (hash, data) = row?;
            let hash: B256 = hash.parse().map_err(|_| eyre::eyre!("invalid hash"))?;
            let exit: ExitIntent = serde_json::from_str(&data)?;
            exits.push((hash, exit));
        }
        Ok(exits)
    }

    /// Mark exits as included in a batch.
    pub fn mark_exits_batched(&self, batch_index: u64, up_to_block: u64) -> eyre::Result<()> {
        self.connection().execute(
            "UPDATE exit_intent SET batch_index = ?1
             WHERE batch_index IS NULL AND zone_block <= ?2",
            (batch_index, up_to_block),
        )?;
        Ok(())
    }

    /// Get exits for a specific batch.
    pub fn get_exits_for_batch(&self, batch_index: u64) -> eyre::Result<Vec<(B256, ExitIntent)>> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT exit_hash, data FROM exit_intent
             WHERE batch_index = ?1
             ORDER BY zone_block ASC, exit_index ASC",
        )?;
        let rows = stmt.query_map([batch_index], |row| {
            let hash: String = row.get(0)?;
            let data: String = row.get(1)?;
            Ok((hash, data))
        })?;
        let mut exits = Vec::new();
        for row in rows {
            let (hash, data) = row?;
            let hash: B256 = hash.parse().map_err(|_| eyre::eyre!("invalid hash"))?;
            let exit: ExitIntent = serde_json::from_str(&data)?;
            exits.push((hash, exit));
        }
        Ok(exits)
    }

    /// Remove exits after a zone block (for revert).
    pub fn remove_exits_after(&self, zone_block: u64) -> eyre::Result<()> {
        self.connection().execute(
            "DELETE FROM exit_intent WHERE zone_block > ?1",
            [zone_block],
        )?;
        Ok(())
    }

    /// Unmark exits from a batch (for L1 reorg).
    pub fn unmark_exits_from_batch(&self, batch_index: u64) -> eyre::Result<()> {
        self.connection().execute(
            "UPDATE exit_intent SET batch_index = NULL WHERE batch_index = ?1",
            [batch_index],
        )?;
        Ok(())
    }

    /// Insert or update account.
    pub fn upsert_account(
        &self,
        address: Address,
        f: impl FnOnce(Option<AccountInfo>) -> eyre::Result<AccountInfo>,
    ) -> eyre::Result<()> {
        upsert_account(&self.connection(), address, f)
    }

    /// Get account by address.
    pub fn get_account(&self, address: Address) -> eyre::Result<Option<AccountInfo>> {
        get_account(&self.connection(), address)
    }
}

fn upsert_account(
    connection: &Connection,
    address: Address,
    f: impl FnOnce(Option<AccountInfo>) -> eyre::Result<AccountInfo>,
) -> eyre::Result<()> {
    let account = get_account(connection, address)?;
    let account = f(account)?;
    connection.execute(
        "INSERT INTO account (address, data) VALUES (?, ?) ON CONFLICT(address) DO UPDATE SET data = excluded.data",
        (address.to_string(), serde_json::to_string(&account)?),
    )?;
    Ok(())
}

fn delete_account(connection: &Connection, address: Address) -> eyre::Result<()> {
    connection.execute("DELETE FROM account WHERE address = ?", (address.to_string(),))?;
    Ok(())
}

fn get_account(connection: &Connection, address: Address) -> eyre::Result<Option<AccountInfo>> {
    match connection.query_row::<String, _, _>(
        "SELECT data FROM account WHERE address = ?",
        (address.to_string(),),
        |row| row.get(0),
    ) {
        Ok(account_info) => Ok(Some(serde_json::from_str(&account_info)?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn upsert_storage(
    connection: &Connection,
    address: Address,
    key: B256,
    data: U256,
) -> eyre::Result<()> {
    connection.execute(
        "INSERT INTO storage (address, key, data) VALUES (?, ?, ?) ON CONFLICT(address, key) DO UPDATE SET data = excluded.data",
        (address.to_string(), key.to_string(), data.to_string()),
    )?;
    Ok(())
}

fn delete_storage(connection: &Connection, address: Address, key: B256) -> eyre::Result<()> {
    connection.execute(
        "DELETE FROM storage WHERE address = ? AND key = ?",
        (address.to_string(), key.to_string()),
    )?;
    Ok(())
}

fn get_storages(connection: &Connection, address: Address) -> eyre::Result<Vec<(B256, U256)>> {
    connection
        .prepare("SELECT key, data FROM storage WHERE address = ?")?
        .query((address.to_string(),))?
        .mapped(|row| {
            Ok((
                B256::from_str(row.get_ref(0)?.as_str()?),
                U256::from_str(row.get_ref(1)?.as_str()?),
            ))
        })
        .map(|result| {
            let (key, data) = result?;
            Ok((key?, data?))
        })
        .collect()
}

fn get_storage(connection: &Connection, address: Address, key: B256) -> eyre::Result<Option<U256>> {
    match connection.query_row::<String, _, _>(
        "SELECT data FROM storage WHERE address = ? AND key = ?",
        (address.to_string(), key.to_string()),
        |row| row.get(0),
    ) {
        Ok(data) => Ok(Some(U256::from_str(&data)?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl reth_revm::Database for Database {
    type Error = PzDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.get_account(address)?)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let bytecode = self.connection().query_row::<String, _, _>(
            "SELECT data FROM bytecode WHERE hash = ?",
            (code_hash.to_string(),),
            |row| row.get(0),
        );
        match bytecode {
            Ok(data) => Ok(Bytecode::new_raw(Bytes::from_str(&data).unwrap_or_default())),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Bytecode::default()),
            Err(err) => Err(PzDbError(err.into())),
        }
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(get_storage(&self.connection(), address, index.into())?.unwrap_or_default())
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        // For now, return zero hash - zone doesn't need L1 block hashes
        Ok(B256::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use rusqlite::Connection;

    fn test_db() -> Database {
        Database::new(Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn test_account_operations() {
        let db = test_db();
        let addr = Address::ZERO;

        assert!(db.get_account(addr).unwrap().is_none());

        db.upsert_account(addr, |_| {
            Ok(AccountInfo {
                balance: U256::from(100),
                nonce: 1,
                ..Default::default()
            })
        })
        .unwrap();

        let acc = db.get_account(addr).unwrap().unwrap();
        assert_eq!(acc.balance, U256::from(100));
        assert_eq!(acc.nonce, 1);
    }

    #[test]
    fn test_database_trait() {
        let mut db = test_db();

        db.upsert_account(Address::ZERO, |_| {
            Ok(AccountInfo {
                balance: U256::from(1000),
                ..Default::default()
            })
        })
        .unwrap();

        let info = reth_revm::Database::basic(&mut db, Address::ZERO)
            .unwrap()
            .unwrap();
        assert_eq!(info.balance, U256::from(1000));
    }

    #[test]
    fn test_zone_state() {
        let db = test_db();

        // Default state
        let state = db.get_zone_state().unwrap();
        assert_eq!(state.zone_block, 0);
        assert_eq!(state.cursor, L1Cursor::default());

        // Update state
        let mut new_state = PzState::default();
        new_state.zone_block = 5;
        new_state.cursor = L1Cursor::new(100, 3);
        new_state.journal_hash = B256::repeat_byte(0x42);
        db.set_zone_state(&new_state).unwrap();

        let state = db.get_zone_state().unwrap();
        assert_eq!(state.zone_block, 5);
        assert_eq!(state.cursor.block_number, 100);
        assert_eq!(state.cursor.log_index, 3);
        assert_eq!(state.journal_hash, B256::repeat_byte(0x42));
    }

    #[test]
    fn test_deposit_operations() {
        let db = test_db();
        let cursor1 = L1Cursor::new(100, 0);
        let cursor2 = L1Cursor::new(100, 1);
        let cursor3 = L1Cursor::new(101, 0);

        let deposit1 = Deposit {
            l1_block_hash: B256::ZERO,
            l1_block_number: 100,
            l1_timestamp: 1000,
            sender: address!("1111111111111111111111111111111111111111"),
            to: address!("2222222222222222222222222222222222222222"),
            amount: U256::from(1000),
            memo: B256::ZERO,
        };

        let deposit2 = Deposit {
            l1_block_number: 100,
            amount: U256::from(2000),
            ..deposit1.clone()
        };

        // Queue deposits
        db.queue_deposit(cursor1, &deposit1, B256::repeat_byte(0x01)).unwrap();
        db.queue_deposit(cursor2, &deposit2, B256::repeat_byte(0x02)).unwrap();

        // Get pending
        let pending = db.get_pending_deposits().unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].0, cursor1);
        assert_eq!(pending[1].0, cursor2);

        // Mark first as processed
        db.mark_deposits_processed(1, cursor1).unwrap();
        let pending = db.get_pending_deposits().unwrap();
        assert_eq!(pending.len(), 1);

        // Queue another and remove after cursor
        db.queue_deposit(cursor3, &deposit1, B256::repeat_byte(0x03)).unwrap();
        db.remove_deposits_after(L1Cursor::new(100, 1)).unwrap();
        let pending = db.get_pending_deposits().unwrap();
        assert_eq!(pending.len(), 1); // Only cursor2 remains
    }

    #[test]
    fn test_journal_operations() {
        let db = test_db();

        // Insert journal entries
        db.insert_journal(1, B256::repeat_byte(0x01), L1Cursor::new(100, 0)).unwrap();
        db.insert_journal(2, B256::repeat_byte(0x02), L1Cursor::new(101, 0)).unwrap();
        db.insert_journal(3, B256::repeat_byte(0x03), L1Cursor::new(102, 0)).unwrap();

        // Get journal hash
        assert_eq!(db.get_journal_hash(2).unwrap(), Some(B256::repeat_byte(0x02)));
        assert_eq!(db.get_journal_hash(99).unwrap(), None);

        // Remove after block 1
        db.remove_journals_after(1).unwrap();
        assert_eq!(db.get_journal_hash(1).unwrap(), Some(B256::repeat_byte(0x01)));
        assert_eq!(db.get_journal_hash(2).unwrap(), None);
        assert_eq!(db.get_journal_hash(3).unwrap(), None);
    }

    #[test]
    fn test_cursor_ordering() {
        let c1 = L1Cursor::new(100, 0);
        let c2 = L1Cursor::new(100, 1);
        let c3 = L1Cursor::new(101, 0);

        assert!(c2.is_after(&c1));
        assert!(c3.is_after(&c2));
        assert!(c3.is_after(&c1));
        assert!(!c1.is_after(&c2));
        assert!(!c1.is_after(&c1));
    }

    #[test]
    fn test_exit_operations() {
        let db = test_db();

        let exit1 = ExitIntent {
            sender: address!("1111111111111111111111111111111111111111"),
            recipient: address!("2222222222222222222222222222222222222222"),
            amount: U256::from(1000),
            zone_block: 1,
            exit_index: 0,
        };
        let exit2 = ExitIntent {
            exit_index: 1,
            amount: U256::from(2000),
            ..exit1.clone()
        };
        let exit3 = ExitIntent {
            zone_block: 2,
            exit_index: 0,
            amount: U256::from(3000),
            ..exit1.clone()
        };

        // Insert exits
        db.insert_exit(&exit1, B256::repeat_byte(0x01)).unwrap();
        db.insert_exit(&exit2, B256::repeat_byte(0x02)).unwrap();
        db.insert_exit(&exit3, B256::repeat_byte(0x03)).unwrap();

        // Get pending (all should be pending)
        let pending = db.get_pending_exits().unwrap();
        assert_eq!(pending.len(), 3);

        // Mark block 1 exits as batched
        db.mark_exits_batched(1, 1).unwrap();
        let pending = db.get_pending_exits().unwrap();
        assert_eq!(pending.len(), 1); // Only exit3 remains

        // Get exits for batch 1
        let batch_exits = db.get_exits_for_batch(1).unwrap();
        assert_eq!(batch_exits.len(), 2);

        // Remove exits after block 1
        db.remove_exits_after(1).unwrap();
        let pending = db.get_pending_exits().unwrap();
        assert!(pending.is_empty());
    }
}
