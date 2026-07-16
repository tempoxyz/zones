//! REVM database adapters backed by stateless trie witnesses.

use std::sync::Arc;

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable as _;
use revm::{
    Database,
    database::states::bundle_state::BundleState,
    precompile::PrecompileError,
    primitives::{AddressMap, B256Map, U256Map},
    state::{AccountInfo, Bytecode},
};
use tempo_primitives::TempoHeader;
use zone_precompiles::{L1StorageReader, SequencerExt};

use crate::{
    Error, StatelessSparseTrieError, TempoStateWitness, ZoneStateWitness, mpt::StatelessSparseTrie,
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
    /// REVM requested a block hash not supplied by the witness.
    #[error("missing block hash in witness: {number}")]
    MissingBlockHash { number: u64 },
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
    pre_state_root: B256,
    node_pool: Vec<Bytes>,
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
        let state = StatelessSparseTrie::new(state_root, &node_pool)?;
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
            pre_state_root: state_root,
            node_pool,
            accounts: AddressMap::default(),
            storage: AddressMap::default(),
            code_by_hash,
        })
    }

    /// Calculate the post-state root from the initial witness and cumulative
    /// in-memory execution changes.
    pub(crate) fn state_root(
        &self,
        bundle_state: &BundleState,
    ) -> Result<B256, StatelessSparseTrieError> {
        let mut trie = StatelessSparseTrie::new(self.pre_state_root, &self.node_pool)?;
        let state = reth_trie_common::HashedPostState::from_bundle_state::<
            reth_trie_common::KeccakKeyHasher,
        >(bundle_state.state());
        trie.calculate_state_root(state)
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
        Err(WitnessDatabaseError::MissingBlockHash { number })
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
    node_pool: Arc<Vec<Bytes>>,
    sequencer: Option<Address>,
}

impl TempoWitnessDatabase {
    /// Construct the reader for the initial Tempo checkpoint.
    pub fn from_tempo_state_witness(witness: TempoStateWitness) -> Result<Self, Error> {
        let node_pool = Arc::new(witness.node_pool);
        let (state, tempo_block_hash, tempo_block_number) =
            checkpoint_state(&witness.initial_tempo_header_rlp, node_pool.as_ref())?;

        Ok(Self {
            state,
            tempo_block_hash,
            tempo_block_number,
            node_pool,
            sequencer: None,
        })
    }

    /// Return a reader rooted at a Tempo header imported by the current Zone
    /// block. `ZoneInbox.advanceTempo` validates that the header is the next
    /// checkpoint before any Tempo-dependent system work executes.
    pub(crate) fn with_imported_checkpoint(
        &self,
        header_rlp: &alloy_primitives::Bytes,
    ) -> Result<Self, Error> {
        let (state, tempo_block_hash, tempo_block_number) =
            checkpoint_state(header_rlp, self.node_pool.as_ref())?;

        Ok(Self {
            state,
            tempo_block_hash,
            tempo_block_number,
            node_pool: self.node_pool.clone(),
            sequencer: self.sequencer,
        })
    }

    /// Attach the Zone's registered sequencer while creating one EVM instance.
    pub(crate) fn for_sequencer(&self, sequencer: Address) -> Self {
        Self {
            sequencer: Some(sequencer),
            ..self.clone()
        }
    }

    /// Returns the checkpoint committed by the decoded initial Tempo header.
    pub(crate) fn checkpoint(&self) -> (u64, B256) {
        (self.tempo_block_number, self.tempo_block_hash)
    }
}

fn checkpoint_state(
    header_rlp: &[u8],
    node_pool: &[Bytes],
) -> Result<(Option<Arc<StatelessSparseTrie>>, B256, u64), Error> {
    let mut encoded_header = header_rlp;
    let header = TempoHeader::decode(&mut encoded_header)
        .map_err(|_| WitnessDatabaseError::InvalidTempoHeader)?;
    if !encoded_header.is_empty() {
        return Err(WitnessDatabaseError::InvalidTempoHeader.into());
    }

    let state_root = header.state_root();
    let state = match StatelessSparseTrie::new(state_root, node_pool) {
        Ok(state) => Some(Arc::new(state)),
        Err(StatelessSparseTrieError::MissingStateRootNode { .. }) => None,
        Err(error) => return Err(error.into()),
    };

    Ok((state, keccak256(header_rlp), header.number()))
}

impl L1StorageReader for TempoWitnessDatabase {
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        tempo_block_number: u64,
    ) -> Result<B256, PrecompileError> {
        if tempo_block_number != self.tempo_block_number {
            return Err(PrecompileError::Fatal(format!(
                "Tempo witness has no root for checkpoint {tempo_block_number}"
            )));
        }

        let state = self.state.as_ref().ok_or_else(|| {
            PrecompileError::Fatal(format!(
                "Tempo witness has no root for checkpoint {tempo_block_number}"
            ))
        })?;
        let value = state
            .storage(account, U256::from_be_bytes(slot.0))
            .map_err(|error| {
                PrecompileError::Fatal(format!(
                    "invalid or incomplete Tempo witness for account {account:?} at slot {slot:?}: {error}"
                ))
            })?;
        Ok(B256::from(value.to_be_bytes::<32>()))
    }
}

impl SequencerExt for TempoWitnessDatabase {
    fn latest_sequencer(&self) -> Option<Address> {
        self.sequencer
    }
}
