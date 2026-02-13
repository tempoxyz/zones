//! Witness-backed database for the zone prover.
//!
//! Implements revm's [`Database`] trait using data from [`ZoneStateWitness`].
//! All account and storage lookups are served from the witness. Any access
//! to an account or storage slot NOT present in the witness is a hard error,
//! preventing the prover from omitting non-zero state.

use alloy_primitives::{Address, B256, U256, map::HashMap};
use revm::{
    Database,
    state::{AccountInfo, Bytecode},
};

use crate::{
    mpt,
    types::{ProverError, ZoneStateWitness},
};

/// A database backed by zone state witness data.
///
/// On construction, all MPT proofs are verified against the provided state root.
/// After verification, accounts and storage are served from in-memory maps.
///
/// The database also tracks mutations during EVM execution so the final
/// state root can be computed.
#[derive(Debug, Clone)]
pub struct WitnessDatabase {
    /// Verified account data, keyed by address.
    accounts: HashMap<Address, VerifiedAccount>,

    /// Code indexed by code hash for `code_by_hash` lookups.
    code_by_hash: HashMap<B256, Bytecode>,

    /// The initial state root (before execution).
    state_root: B256,
}

/// Internal representation of a verified account.
#[derive(Debug, Clone)]
struct VerifiedAccount {
    info: AccountInfo,
    storage: HashMap<U256, U256>,
    /// The verified storage root (from the account trie).
    /// Used during full MPT verification but not yet read in the stub path.
    #[allow(dead_code)]
    storage_root: B256,
}

impl WitnessDatabase {
    /// Create a new `WitnessDatabase` from a zone state witness.
    ///
    /// Verifies all MPT proofs for accounts and storage slots against the
    /// witness state root. Returns an error if any proof is invalid.
    pub fn from_witness(witness: &ZoneStateWitness) -> Result<Self, ProverError> {
        let mut accounts = HashMap::default();
        let mut code_by_hash: HashMap<B256, Bytecode> = HashMap::default();

        for (addr, acct) in &witness.accounts {
            // Use the storage_root provided in the witness.
            // The account proof verification below validates that this storage_root
            // is correct (it's part of the account RLP in the state trie).
            let storage_root = acct.storage_root;

            // Verify the account proof against the state root.
            mpt::verify_account_proof(
                witness.state_root,
                *addr,
                acct.nonce,
                acct.balance,
                storage_root,
                acct.code_hash,
                &acct.account_proof,
            )?;

            // Verify storage proofs.
            for (slot, proof) in &acct.storage_proofs {
                let value = acct.storage.get(slot).copied().unwrap_or(U256::ZERO);
                mpt::verify_storage_proof(storage_root, *slot, value, proof)?;
            }

            // Build AccountInfo for revm.
            let info = AccountInfo {
                balance: acct.balance,
                nonce: acct.nonce,
                code_hash: acct.code_hash,
                code: acct.code.as_ref().map(|c| {
                    Bytecode::new_raw(alloy_primitives::Bytes::copy_from_slice(c))
                }),
            };

            // Index code by hash.
            if let Some(code) = &info.code {
                code_by_hash.insert(acct.code_hash, code.clone());
            }

            accounts.insert(
                *addr,
                VerifiedAccount {
                    info,
                    storage: acct.storage.clone(),
                    storage_root,
                },
            );
        }

        Ok(Self {
            accounts,
            code_by_hash,
            state_root: witness.state_root,
        })
    }

    /// Returns the initial state root.
    pub fn state_root(&self) -> B256 {
        self.state_root
    }
}

impl Database for WitnessDatabase {
    type Error = ProverError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        match self.accounts.get(&address) {
            Some(acct) => Ok(Some(acct.info.clone())),
            // Per spec: "Any account or storage access not present in the witness
            // must be treated as an error (do not default to zero)."
            None => Err(ProverError::MissingWitness(format!(
                "account {address} not in witness"
            ))),
        }
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.code_by_hash
            .get(&code_hash)
            .cloned()
            .ok_or_else(|| {
                ProverError::MissingWitness(format!("code hash {code_hash} not in witness"))
            })
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let acct = self.accounts.get(&address).ok_or_else(|| {
            ProverError::MissingWitness(format!(
                "account {address} not in witness (storage read)"
            ))
        })?;

        // Storage slots must be in the witness. Missing = error, not zero.
        acct.storage.get(&index).copied().ok_or_else(|| {
            ProverError::MissingWitness(format!(
                "storage slot {index} for account {address} not in witness"
            ))
        })
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        // Zone blocks don't use BLOCKHASH across chains.
        // Return zero for now; if needed, the witness can include block hashes.
        Ok(B256::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, address};

    use super::*;

    /// Test that accessing a missing account returns an error (not zero).
    #[test]
    fn test_missing_account_errors() {
        let witness = ZoneStateWitness {
            accounts: HashMap::default(),
            state_root: B256::ZERO,
        };

        // We can't verify proofs with an empty state root and no accounts,
        // so create the database directly for this unit test.
        let mut db = WitnessDatabase {
            accounts: HashMap::default(),
            code_by_hash: HashMap::default(),
            state_root: B256::ZERO,
        };

        let result = db.basic(address!("0x0000000000000000000000000000000000000001"));
        assert!(result.is_err());
    }

    /// Test that accessing a missing storage slot returns an error.
    #[test]
    fn test_missing_storage_errors() {
        let addr = address!("0x0000000000000000000000000000000000000001");
        let mut storage = HashMap::default();
        storage.insert(U256::from(1), U256::from(42));

        let mut accounts = HashMap::default();
        accounts.insert(
            addr,
            VerifiedAccount {
                info: AccountInfo::default(),
                storage,
                storage_root: mpt::empty_storage_root(),
            },
        );

        let mut db = WitnessDatabase {
            accounts,
            code_by_hash: HashMap::default(),
            state_root: B256::ZERO,
        };

        // Slot 1 exists
        assert_eq!(db.storage(addr, U256::from(1)).unwrap(), U256::from(42));

        // Slot 2 does not — must error
        assert!(db.storage(addr, U256::from(2)).is_err());
    }
}
