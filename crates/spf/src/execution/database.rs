//! REVM database adapters backed by stateless trie witnesses.

use std::sync::{Arc, Mutex};

use alloy_consensus::BlockHeader as _;
use alloy_eips::eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS};
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rlp::Decodable as _;
use revm::{
    Database,
    database::states::bundle_state::BundleState,
    primitives::{AddressMap, B256Map, U256Map},
    state::{AccountInfo, Bytecode},
};
use tempo_primitives::TempoHeader;
use tracing::error;
use zone_precompiles::{L1StateError, L1StorageReader};

use crate::{
    Error, StatelessSparseTrieError, TempoStateWitness, ZoneStateWitness,
    mpt::{IndexedTrieNodePool, StatelessSparseTrie},
};

/// Errors emitted while resolving an execution read against a witness.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessDatabaseError {
    /// A state path could not be resolved from the Zone or Tempo node pool.
    #[error(transparent)]
    Mpt(#[from] StatelessSparseTrieError),
    /// REVM requested bytecode not supplied by the Zone witness.
    #[error("missing bytecode in witness: {code_hash:?}")]
    MissingCode { code_hash: B256 },
    /// The Zone witness supplied the same bytecode preimage more than once.
    #[error("duplicate bytecode hash in Zone state witness: {code_hash:?}")]
    DuplicateBytecodeHash { code_hash: B256 },
    /// The execution inputs assigned two different hashes to one Zone block number.
    #[error("conflicting block hash for {number}: expected {expected:?}, got {actual:?}")]
    ConflictingBlockHash {
        number: u64,
        expected: B256,
        actual: B256,
    },
    /// The initial Tempo header is not a complete RLP-encoded Tempo header.
    #[error("invalid initial Tempo header in witness")]
    InvalidTempoHeader,
}

impl revm::database_interface::DBErrorMarker for WitnessDatabaseError {}

/// REVM database backed by a root-bound, fully revealed Zone state trie.
#[derive(Debug)]
pub struct WitnessDatabase {
    state: StatelessSparseTrie,
    accounts: AddressMap<Option<AccountInfo>>,
    storage: AddressMap<U256Map<U256>>,
    code_by_hash: B256Map<Bytecode>,
}

impl WitnessDatabase {
    /// Create a Zone execution database rooted at the parent Zone header's
    /// state root.
    ///
    /// The node pool is fully revealed and checked against `state_root` before
    /// this returns. Bytecode is looked up by the code hash proven in an
    /// account leaf.
    pub fn from_zone_state_witness(
        witness: ZoneStateWitness,
        state_root: B256,
    ) -> Result<Self, Error> {
        let ZoneStateWitness {
            node_pool,
            bytecodes,
        } = witness;
        let state = StatelessSparseTrie::new(state_root, node_pool)?;
        let mut code_by_hash = B256Map::default();
        for code in bytecodes {
            let code_hash = keccak256(&code);
            if code_by_hash
                .insert(code_hash, Bytecode::new_raw(code))
                .is_some()
            {
                return Err(WitnessDatabaseError::DuplicateBytecodeHash { code_hash }.into());
            }
        }

        Ok(Self {
            state,
            accounts: AddressMap::default(),
            storage: AddressMap::default(),
            code_by_hash,
        })
    }

    /// Apply one block's execution changes to the current Zone state trie and
    /// return the resulting post-state root.
    pub(crate) fn state_root(
        &mut self,
        bundle_state: BundleState,
    ) -> Result<B256, StatelessSparseTrieError> {
        // Advance the trie from the previous block's root using this block's changes.
        let state = reth_trie_common::HashedPostState::from_bundle_state::<
            reth_trie_common::KeccakKeyHasher,
        >(bundle_state.state());
        let state_root = self.state.calculate_state_root(state)?;

        // Keep database read caches coherent with the newly advanced trie.
        for (address, account) in bundle_state.state() {
            self.accounts.insert(*address, account.info.clone());

            if account.status.is_storage_known() {
                self.storage.remove(address);
            }

            let storage_entry = self.storage.entry(*address).or_default();
            for (slot, value) in account.storage.iter() {
                storage_entry.insert(*slot, value.present_value);
            }
        }

        Ok(state_root)
    }
}

impl Database for WitnessDatabase {
    type Error = WitnessDatabaseError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(account) = self.accounts.get(&address) {
            return Ok(account.clone());
        }

        let account = self.state.account(address)?.map(|account| AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
            account_id: None,
            code: None,
        });
        self.accounts.insert(address, account.clone());
        Ok(account)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.code_by_hash
            .get(&code_hash)
            .cloned()
            .ok_or(WitnessDatabaseError::MissingCode { code_hash })
    }

    fn storage(&mut self, address: Address, slot: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self
            .storage
            .get(&address)
            .and_then(|slots| slots.get(&slot))
        {
            return Ok(*value);
        }

        let value = self.state.storage(address, slot)?;
        self.storage.entry(address).or_default().insert(slot, value);
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        // EIP-2935 makes historical block hashes part of the authenticated Zone state.
        // Resolve BLOCKHASH through the history contract so the ordinary storage witness
        // proves the returned value against the parent header's state root.
        let slot = U256::from(number % HISTORY_SERVE_WINDOW as u64);
        let value = self.storage(HISTORY_STORAGE_ADDRESS, slot)?;
        Ok(B256::from(value.to_be_bytes::<32>()))
    }
}

/// Tempo state reader for the checkpoint header supplied in the witness.
///
/// When the shared node pool includes that checkpoint's root, it owns a fully
/// revealed immutable sparse trie. Otherwise it remains inactive and rejects
/// any Tempo storage read as an incomplete witness.
#[derive(Clone, Debug)]
pub struct TempoWitnessDatabase {
    state: Option<Arc<StatelessSparseTrie>>,
    tempo_block_hash: B256,
    tempo_block_number: u64,
    node_pool: Arc<IndexedTrieNodePool>,
    missing_read: Arc<Mutex<Option<MissingTempoStorageRead>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MissingTempoStorageRead {
    pub(crate) account: Address,
    pub(crate) slot: B256,
    pub(crate) block_number: u64,
}

impl TempoWitnessDatabase {
    /// Construct the reader for the initial Tempo checkpoint.
    pub fn from_tempo_state_witness(witness: TempoStateWitness) -> Result<Self, Error> {
        let (tempo_header, tempo_block_hash) = decode_header(&witness.initial_tempo_header_rlp)?;
        let node_pool = Arc::new(IndexedTrieNodePool::new(witness.node_pool)?);
        let state = match StatelessSparseTrie::new_with_node_pool(
            tempo_header.state_root(),
            node_pool.as_ref(),
        ) {
            Ok(state) => Some(Arc::new(state)),
            Err(StatelessSparseTrieError::MissingStateRootNode { .. }) => None,
            Err(error) => return Err(error.into()),
        };

        Ok(Self {
            state,
            tempo_block_hash,
            tempo_block_number: tempo_header.number(),
            node_pool,
            missing_read: Arc::default(),
        })
    }

    /// Return a reader rooted at a Tempo header imported by the current Zone
    /// block. `ZoneInbox.advanceTempo` validates that the header is the next
    /// checkpoint before any Tempo-dependent system work executes.
    pub(crate) fn with_imported_checkpoint(
        self,
        header_rlp: &alloy_primitives::Bytes,
    ) -> Result<Self, Error> {
        let (tempo_header, tempo_block_hash) = decode_header(header_rlp)?;

        let mut state = match self.state.map(Arc::try_unwrap) {
            Some(Ok(state)) => state,
            Some(Err(_)) => {
                error!("failed to unwrap old Tempo trie state, creating a new one");
                StatelessSparseTrie::default()
            }
            None => StatelessSparseTrie::default(),
        };
        let state = match state.reset(tempo_header.state_root(), self.node_pool.as_ref()) {
            Ok(()) => Some(Arc::new(state)),
            Err(StatelessSparseTrieError::MissingStateRootNode { .. }) => None,
            Err(error) => return Err(error.into()),
        };

        Ok(Self {
            state,
            tempo_block_hash,
            tempo_block_number: tempo_header.number(),
            node_pool: self.node_pool,
            missing_read: self.missing_read,
        })
    }

    /// Returns the checkpoint committed by the decoded initial Tempo header.
    pub(crate) fn checkpoint(&self) -> (u64, B256) {
        (self.tempo_block_number, self.tempo_block_hash)
    }

    pub(crate) fn missing_read(&self) -> Option<MissingTempoStorageRead> {
        *self
            .missing_read
            .lock()
            .expect("missing Tempo storage read mutex poisoned")
    }

    fn record_missing_read(&self, account: Address, slot: B256, block_number: u64) {
        let mut missing = self
            .missing_read
            .lock()
            .expect("missing Tempo storage read mutex poisoned");
        missing.get_or_insert(MissingTempoStorageRead {
            account,
            slot,
            block_number,
        });
    }
}

/// Decodes a Tempo header from its RLP-encoded form, returning the header and its hash.
fn decode_header(header_rlp: &[u8]) -> Result<(TempoHeader, B256), Error> {
    let mut encoded_header = header_rlp;
    let header = TempoHeader::decode(&mut encoded_header)
        .map_err(|_| WitnessDatabaseError::InvalidTempoHeader)?;
    if !encoded_header.is_empty() {
        return Err(WitnessDatabaseError::InvalidTempoHeader.into());
    }

    Ok((header, keccak256(header_rlp)))
}

impl L1StorageReader for TempoWitnessDatabase {
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        tempo_block_number: u64,
    ) -> Result<B256, L1StateError> {
        if tempo_block_number != self.tempo_block_number {
            return Err(storage_unavailable(
                account,
                slot,
                tempo_block_number,
                "witness has no root for the requested checkpoint",
            ));
        }

        let state = self.state.as_ref().ok_or_else(|| {
            self.record_missing_read(account, slot, tempo_block_number);
            storage_unavailable(
                account,
                slot,
                tempo_block_number,
                "witness does not include the checkpoint state root",
            )
        })?;
        let value = match state.storage(account, U256::from_be_bytes(slot.0)) {
            Ok(value) => value,
            Err(
                error @ (StatelessSparseTrieError::IncompleteAccountProof { .. }
                | StatelessSparseTrieError::IncompleteStorageProof { .. }),
            ) => {
                self.record_missing_read(account, slot, tempo_block_number);
                return Err(L1StateError::StorageUnavailable {
                    account,
                    slot,
                    block_number: tempo_block_number,
                    reason: format!("incomplete Tempo witness: {error}"),
                });
            }
            Err(error) => {
                return Err(L1StateError::StorageUnavailable {
                    account,
                    slot,
                    block_number: tempo_block_number,
                    reason: format!("invalid Tempo witness: {error}"),
                });
            }
        };
        Ok(B256::from(value.to_be_bytes::<32>()))
    }
}

fn storage_unavailable(
    account: Address,
    slot: B256,
    block_number: u64,
    reason: &'static str,
) -> L1StateError {
    L1StateError::StorageUnavailable {
        account,
        slot,
        block_number,
        reason: reason.into(),
    }
}
