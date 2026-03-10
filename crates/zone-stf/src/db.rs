//! Witness-backed [`Database`] for revm execution inside the prover.
//!
//! [`WitnessDb`] serves account info and storage values from a
//! [`ZoneStateWitness`] that was pre-collected on the host. State changes
//! are accumulated in-memory via [`DatabaseCommit`] so that later
//! transactions see the effects of earlier ones.

use alloc::collections::BTreeMap;
use alloy_primitives::{Address, B256, Bytes, U256};
use revm::{
    Database, DatabaseCommit,
    bytecode::Bytecode,
    state::AccountInfo,
};
use zone_primitives::ZoneStateWitness;

/// Error returned by [`WitnessDb`] when the EVM accesses data that was not
/// included in the witness.
#[derive(Debug)]
pub enum WitnessDbError {
    /// The requested code hash has no corresponding bytecode in the witness.
    MissingCode(B256),
}

impl core::fmt::Display for WitnessDbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingCode(h) => write!(f, "code hash {h} not in witness"),
        }
    }
}

impl core::error::Error for WitnessDbError {}
impl revm::database_interface::DBErrorMarker for WitnessDbError {}

// ---------------------------------------------------------------------------
//  Internal types
// ---------------------------------------------------------------------------

/// Cached account data built from the witness.
struct WitnessAccount {
    nonce: u64,
    balance: U256,
    code_hash: B256,
    code: Option<Bytecode>,
    storage: BTreeMap<U256, U256>,
}

// ---------------------------------------------------------------------------
//  WitnessDb
// ---------------------------------------------------------------------------

/// Revm [`Database`] backed by a [`ZoneStateWitness`].
///
/// Reads return data from the witness. Commits from EVM execution are
/// stored in-memory so subsequent reads reflect accumulated state changes.
pub struct WitnessDb {
    /// Account data indexed by address.
    accounts: BTreeMap<Address, WitnessAccount>,
    /// Bytecode indexed by code hash (for `code_by_hash` lookups).
    code_by_hash: BTreeMap<B256, Bytecode>,
    /// Zone state root at the start of the batch.
    state_root: B256,
}

impl WitnessDb {
    /// Build from the initial zone state witness.
    pub fn from_witness(witness: &ZoneStateWitness) -> Self {
        let mut accounts = BTreeMap::new();
        let mut code_by_hash = BTreeMap::new();

        for (addr, aw) in &witness.accounts {
            let bytecode = aw.code.as_ref().map(|c| {
                Bytecode::new_raw(Bytes::copy_from_slice(c))
            });
            if let Some(ref bc) = bytecode {
                code_by_hash.insert(aw.code_hash, bc.clone());
            }

            let storage = aw.storage.iter().map(|(k, v)| (*k, *v)).collect();

            accounts.insert(
                *addr,
                WitnessAccount {
                    nonce: aw.nonce,
                    balance: aw.balance,
                    code_hash: aw.code_hash,
                    code: bytecode,
                    storage,
                },
            );
        }

        Self {
            accounts,
            code_by_hash,
            state_root: witness.state_root,
        }
    }

    /// Current zone state root.
    ///
    /// TODO: recompute from committed changes via MPT update. Currently returns
    /// the initial state root.
    pub fn state_root(&self) -> B256 {
        self.state_root
    }

    /// Read a storage value from the current (possibly committed) state.
    pub fn read_storage(&self, address: Address, slot: U256) -> U256 {
        self.accounts
            .get(&address)
            .and_then(|a| a.storage.get(&slot))
            .copied()
            .unwrap_or(U256::ZERO)
    }
}

// ---------------------------------------------------------------------------
//  Database impl
// ---------------------------------------------------------------------------

impl Database for WitnessDb {
    type Error = WitnessDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let Some(account) = self.accounts.get(&address) else {
            return Ok(None);
        };
        Ok(Some(AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
            code: account.code.clone(),
            ..Default::default()
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if code_hash == B256::ZERO || code_hash == revm::primitives::KECCAK_EMPTY {
            return Ok(Bytecode::default());
        }
        self.code_by_hash
            .get(&code_hash)
            .cloned()
            .ok_or(WitnessDbError::MissingCode(code_hash))
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(account) = self.accounts.get(&address) {
            return Ok(account.storage.get(&index).copied().unwrap_or(U256::ZERO));
        }
        Ok(U256::ZERO)
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        // Zone blocks don't rely on the BLOCKHASH opcode.
        Ok(B256::ZERO)
    }
}

// ---------------------------------------------------------------------------
//  DatabaseCommit impl
// ---------------------------------------------------------------------------

impl DatabaseCommit for WitnessDb {
    fn commit(&mut self, changes: revm::primitives::HashMap<Address, revm::state::Account>) {
        for (addr, account) in changes {
            let entry = self.accounts.entry(addr).or_insert_with(|| WitnessAccount {
                nonce: 0,
                balance: U256::ZERO,
                code_hash: revm::primitives::KECCAK_EMPTY,
                code: None,
                storage: BTreeMap::new(),
            });

            entry.nonce = account.info.nonce;
            entry.balance = account.info.balance;
            entry.code_hash = account.info.code_hash;

            if let Some(code) = account.info.code {
                self.code_by_hash.insert(entry.code_hash, code.clone());
                entry.code = Some(code);
            }

            for (slot, evm_slot) in account.storage {
                entry.storage.insert(slot, evm_slot.present_value);
            }
        }
    }
}
