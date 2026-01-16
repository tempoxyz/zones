//! SQL database for Privacy Zone state with revm::Database implementation.
//!
//! Based on reth-exex-examples/rollup pattern.

use crate::error::PzDbError;
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
            CREATE TABLE IF NOT EXISTS deposit (
                id              INTEGER PRIMARY KEY,
                block_number    TEXT,
                deposit_hash    TEXT UNIQUE,
                data            TEXT
            );
            CREATE TABLE IF NOT EXISTS zone_state (
                id   INTEGER PRIMARY KEY,
                data TEXT
            );",
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
}
