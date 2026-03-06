//! Witness-backed database for the zone prover.
//!
//! Implements revm's [`Database`] trait using data from [`ZoneStateWitness`].
//! All account and storage lookups are served from the witness. Any access
//! to an account or storage slot NOT present in the witness is a hard error,
//! preventing the prover from omitting non-zero state.

use alloy_primitives::{
    Address, B256, KECCAK256_EMPTY, U256, keccak256,
    map::{HashMap, HashSet},
};
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

    /// Addresses confirmed absent from the state trie (verified by exclusion proof).
    absent_accounts: HashSet<Address>,

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
    /// Verified during construction and retained for diagnostics.
    _storage_root: B256,
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

            // If bytecode is provided, enforce its hash matches the account proof.
            if let Some(code) = &acct.code {
                let actual_hash = keccak256(code.as_ref());
                if actual_hash != acct.code_hash {
                    return Err(ProverError::InvalidProof(format!(
                        "code hash mismatch for account {addr}: expected={}, actual={actual_hash}",
                        acct.code_hash,
                    )));
                }
            }

            // Enforce a 1:1 mapping between storage values and storage proofs.
            if acct.storage.len() != acct.storage_proofs.len()
                || acct
                    .storage
                    .keys()
                    .any(|slot| !acct.storage_proofs.contains_key(slot))
                || acct
                    .storage_proofs
                    .keys()
                    .any(|slot| !acct.storage.contains_key(slot))
            {
                return Err(ProverError::InvalidProof(format!(
                    "storage witness/proof mismatch for account {addr}"
                )));
            }

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
                code: acct
                    .code
                    .as_ref()
                    .map(|c| Bytecode::new_raw(alloy_primitives::Bytes::copy_from_slice(c))),
                account_id: Some(0),
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
                    _storage_root: storage_root,
                },
            );
        }

        // Verify absence proofs for confirmed-absent accounts.
        let mut absent_accounts = HashSet::default();
        for (addr, proof) in &witness.absent_accounts {
            mpt::verify_account_absence_proof(witness.state_root, *addr, proof)?;
            absent_accounts.insert(*addr);
        }

        Ok(Self {
            accounts,
            absent_accounts,
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
        if let Some(acct) = self.accounts.get(&address) {
            return Ok(Some(acct.info.clone()));
        }
        // Account is confirmed absent (verified by exclusion proof).
        if self.absent_accounts.contains(&address) {
            return Ok(None);
        }
        // Truly missing from the witness — hard error.
        Err(ProverError::MissingWitness(format!(
            "account {address} not in witness"
        )))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        // KECCAK_EMPTY is the code hash for EOAs (no bytecode). revm may call
        // code_by_hash with this value, so return empty bytecode without
        // requiring it in the witness.
        if code_hash == KECCAK256_EMPTY {
            return Ok(Bytecode::default());
        }
        self.code_by_hash.get(&code_hash).cloned().ok_or_else(|| {
            ProverError::MissingWitness(format!("code hash {code_hash} not in witness"))
        })
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        // Absent accounts have no storage — return zero.
        if self.absent_accounts.contains(&address) {
            return Ok(U256::ZERO);
        }

        let acct = self.accounts.get(&address).ok_or_else(|| {
            ProverError::MissingWitness(format!("account {address} not in witness (storage read)"))
        })?;

        // Storage slots must be in the witness. Missing = error, not zero.
        acct.storage.get(&index).copied().ok_or_else(|| {
            ProverError::MissingWitness(format!(
                "storage slot {index} for account {address} not in witness"
            ))
        })
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        // EIP-2935: read from the history storage contract in the witness.
        // The zone node stores parent block hashes in this contract via the
        // pre-execution system call, and the RecordingDatabase ensures the
        // corresponding storage slot is included in the witness proofs.
        //
        // For intra-batch block hashes (blocks executed in the current batch
        // whose writes haven't reached the witness DB), the caller should
        // pre-populate the State's `block_hashes` cache before execution.
        let slot = U256::from(number % alloy_eips::eip2935::HISTORY_SERVE_WINDOW as u64);
        let addr = alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS;

        if self.absent_accounts.contains(&addr) {
            return Ok(B256::ZERO);
        }

        match self.accounts.get(&addr) {
            Some(acct) => {
                let value = acct.storage.get(&slot).copied().ok_or_else(|| {
                    ProverError::MissingWitness(format!(
                        "EIP-2935 slot {slot} missing in witness for block_hash({number})"
                    ))
                })?;
                Ok(B256::from(value.to_be_bytes()))
            }
            // 2935 contract not in witness and not confirmed absent — hard error,
            // consistent with the missing-witness invariant enforced by basic().
            None => Err(ProverError::MissingWitness(format!(
                "EIP-2935 history contract {addr} not in witness (block_hash({number}))"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;
    use crate::testutil::{TestAccount, build_zone_state_fixture};

    /// Test that accessing a missing account returns an error (not zero).
    #[test]
    fn test_missing_account_errors() {
        // We can't verify proofs with an empty state root and no accounts,
        // so create the database directly for this unit test.
        let mut db = WitnessDatabase {
            accounts: HashMap::default(),
            absent_accounts: HashSet::default(),
            code_by_hash: HashMap::default(),
            state_root: B256::ZERO,
        };

        let result = db.basic(address!("0x0000000000000000000000000000000000000001"));
        assert!(result.is_err());
    }

    /// Test that confirmed-absent accounts return Ok(None) instead of error.
    #[test]
    fn test_absent_account_returns_none() {
        let addr = address!("0x0000000000000000000000000000000000000042");
        let mut absent = HashSet::default();
        absent.insert(addr);

        let mut db = WitnessDatabase {
            accounts: HashMap::default(),
            absent_accounts: absent,
            code_by_hash: HashMap::default(),
            state_root: B256::ZERO,
        };

        // basic() returns Ok(None) for absent accounts.
        assert_eq!(db.basic(addr).unwrap(), None);
        // storage() returns zero for absent accounts.
        assert_eq!(db.storage(addr, U256::from(1)).unwrap(), U256::ZERO);
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
                _storage_root: mpt::empty_storage_root(),
            },
        );

        let mut db = WitnessDatabase {
            accounts,
            absent_accounts: HashSet::default(),
            code_by_hash: HashMap::default(),
            state_root: B256::ZERO,
        };

        // Slot 1 exists
        assert_eq!(db.storage(addr, U256::from(1)).unwrap(), U256::from(42));

        // Slot 2 does not — must error
        assert!(db.storage(addr, U256::from(2)).is_err());
    }

    #[test]
    fn test_code_hash_mismatch_errors() {
        let addr = address!("0x1000000000000000000000000000000000000000");
        let code = vec![0x60, 0x00, 0x60, 0x00, 0x52];

        let account = TestAccount {
            nonce: 1,
            balance: U256::from(1),
            code_hash: keccak256(&code),
            code: Some(code),
            storage: vec![],
        };

        let fixture = build_zone_state_fixture(&[(addr, account)]);
        let mut witness = fixture.witness;
        witness
            .accounts
            .get_mut(&addr)
            .expect("account present")
            .code = Some(alloy_primitives::Bytes::from(vec![0x60, 0x01]));

        let result = WitnessDatabase::from_witness(&witness);
        assert!(matches!(result, Err(ProverError::InvalidProof(_))));
    }

    #[test]
    fn test_storage_proof_completeness_errors() {
        let addr = address!("0x2000000000000000000000000000000000000000");
        let account = TestAccount {
            nonce: 1,
            balance: U256::from(1),
            code_hash: KECCAK256_EMPTY,
            code: None,
            storage: vec![(U256::from(1), U256::from(42))],
        };

        let fixture = build_zone_state_fixture(&[(addr, account)]);
        let mut witness = fixture.witness;
        let acct = witness.accounts.get_mut(&addr).expect("account present");
        acct.storage_proofs.remove(&U256::from(1));

        let result = WitnessDatabase::from_witness(&witness);
        assert!(matches!(result, Err(ProverError::InvalidProof(_))));
    }
}
