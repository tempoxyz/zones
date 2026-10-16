//! Durable test-chain journal and real Zone/QMDB execution.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Arc,
};

use alloy_consensus::{BlockHeader as _, Sealable as _, constants::KECCAK_EMPTY};
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use reth_chainspec::EthChainSpec as _;
use reth_trie_common::{EMPTY_ROOT_HASH, TrieAccount};
use revm::Database as _;
use serde::{Deserialize, Serialize};
use tempo_precompiles::{PATH_USD_ADDRESS, storage::StorageKey as _, tip20::tip20_slots};
use tempo_primitives::TempoHeader;
use zone_chainspec::ZoneChainSpec;
use zone_precompiles::tempo_state;
use zone_primitives::constants::{TEMPO_STATE_ADDRESS, zone_chain_id};
use zone_spf::{
    BatchOutput, BatchWitness, PublicInputs, QmdbKey, QmdbMutation, QmdbStateWitness, SpfConfig,
    TempoImport, TempoStateWitness, WitnessDatabase, ZoneBlock, ZoneStateWitness,
    qmdb::{ReplayBlock, execute_qmdb_zone_batch, prove_qmdb_zone_batch, state_root},
};

pub(crate) const DEV_ACCOUNT: Address = address!("f39fd6e51aad88f6f4ce6ab8827279cfffb92266");
const PARENT_CHAIN_ID: u64 = 1337;
const ZONE_ID: u32 = 1;

/// Exported prover input. The genesis must be selected independently by a verifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProofInput {
    pub witness: BatchWitness,
    pub qmdb_state_witness: QmdbStateWitness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBlock {
    witness: BatchWitness,
    executed: ReplayBlock,
    output: BatchOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    genesis: Genesis,
    genesis_header: TempoHeader,
    history: QmdbStateWitness,
    state_witness: ZoneStateWitness,
    tempo_header: TempoHeader,
    blocks: Vec<StoredBlock>,
}

pub(crate) struct Node {
    datadir: PathBuf,
    // Held throughout the node's lifetime. Prevents two processes overwriting the journal.
    _lock: File,
    config: SpfConfig,
    journal: Journal,
}

impl Node {
    pub(crate) fn open(datadir: PathBuf) -> eyre::Result<Self> {
        fs::create_dir_all(&datadir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(datadir.join("LOCK"))?;
        lock.try_lock()
            .map_err(|error| eyre::eyre!("QMDB datadir already locked: {error}"))?;
        let path = datadir.join("chain.json");
        let journal: Journal = if path.exists() {
            serde_json::from_slice(&fs::read(path)?)?
        } else {
            initial_journal()?
        };
        eyre::ensure!(
            journal.version == 1,
            "unsupported QMDB test journal version"
        );
        let config = SpfConfig::new(Arc::new(ZoneChainSpec::from_genesis(
            journal.genesis.clone(),
        )?));
        let node = Self {
            datadir,
            _lock: lock,
            config,
            journal,
        };
        eyre::ensure!(
            node.chain_id() == zone_chain_id(PARENT_CHAIN_ID, ZONE_ID)?,
            "wrong test chain ID"
        );
        // Re-execute from the fixed genesis; do not trust roots, bytecode or bodies loaded from disk.
        let mut expected = initial_journal()?;
        eyre::ensure!(
            node.journal.genesis == expected.genesis
                && node.journal.genesis_header == expected.genesis_header,
            "test genesis mismatch"
        );
        for stored in &node.journal.blocks {
            eyre::ensure!(
                stored.witness.zone_blocks.len() == 1,
                "invalid journal block shape"
            );
            let next = Self::execute(
                &node.config,
                &expected,
                stored.witness.zone_blocks[0].transactions.clone(),
            )?;
            eyre::ensure!(
                next.blocks.last() == Some(stored),
                "journal execution mismatch"
            );
            expected = next;
        }
        eyre::ensure!(node.journal == expected, "journal state mismatch");
        node.save(&node.journal)?;
        Ok(node)
    }

    pub(crate) fn chain_id(&self) -> u64 {
        self.config.chain_spec().chain_id()
    }

    pub(crate) fn height(&self) -> u64 {
        self.journal.blocks.len() as u64
    }

    pub(crate) fn head(&self) -> &TempoHeader {
        self.journal
            .blocks
            .last()
            .map_or(&self.journal.genesis_header, |block| &block.executed.header)
    }

    pub(crate) fn database(&self) -> eyre::Result<WitnessDatabase> {
        Ok(WitnessDatabase::from_qmdb_state_witness(
            self.journal.history.clone(),
            self.journal.state_witness.clone(),
            self.head().state_root(),
        )?)
    }

    pub(crate) fn balance(&self, address: Address) -> eyre::Result<U256> {
        Ok(self
            .database()?
            .basic(address)?
            .map_or(U256::ZERO, |account| account.balance))
    }

    pub(crate) fn nonce(&self, address: Address) -> eyre::Result<u64> {
        Ok(self
            .database()?
            .basic(address)?
            .map_or(0, |account| account.nonce))
    }

    pub(crate) fn storage(&self, address: Address, slot: U256) -> eyre::Result<U256> {
        Ok(self.database()?.storage(address, slot)?)
    }

    pub(crate) fn code(&self, address: Address) -> eyre::Result<Bytes> {
        let mut database = self.database()?;
        let Some(account) = database.basic(address)? else {
            return Ok(Bytes::new());
        };
        Ok(database.code_by_hash(account.code_hash)?.original_bytes())
    }

    pub(crate) fn mine(&mut self, transactions: Vec<Bytes>) -> eyre::Result<B256> {
        let next = Self::execute(&self.config, &self.journal, transactions)?;
        let hash = next.blocks.last().unwrap().executed.header.hash_slow();
        self.save(&next)?;
        self.journal = next;
        Ok(hash)
    }

    pub(crate) fn block(&self, number: u64) -> Option<&ReplayBlock> {
        number
            .checked_sub(1)
            .and_then(|number| self.journal.blocks.get(number as usize))
            .map(|block| &block.executed)
    }

    pub(crate) fn genesis_header(&self) -> &TempoHeader {
        &self.journal.genesis_header
    }

    pub(crate) fn proof_input(&self, number: u64) -> eyre::Result<ProofInput> {
        let stored = self
            .journal
            .blocks
            .get(
                number
                    .checked_sub(1)
                    .ok_or_else(|| eyre::eyre!("genesis has no execution witness"))?
                    as usize,
            )
            .ok_or_else(|| eyre::eyre!("block not found"))?;
        Ok(ProofInput {
            witness: stored.witness.clone(),
            qmdb_state_witness: QmdbStateWitness {
                batches: self.journal.history.batches[..number as usize].to_vec(),
            },
        })
    }

    pub(crate) fn prove(&self, number: u64) -> eyre::Result<BatchOutput> {
        let input = self.proof_input(number)?;
        let output = prove_qmdb_zone_batch(&self.config, input.witness, input.qmdb_state_witness)?;
        eyre::ensure!(
            output == self.journal.blocks[number as usize - 1].output,
            "proof output mismatch"
        );
        Ok(output)
    }

    pub(crate) fn checkpoint(&self) -> serde_json::Value {
        serde_json::json!({"genesis": self.journal.genesis, "header": self.head(),
            "history": self.journal.history, "stateWitness": self.journal.state_witness,
            "mockL1": true, "attested": false})
    }

    pub(crate) fn read_proof(&self, key: QmdbKey) -> eyre::Result<zone_spf::qmdb::QmdbReadProof> {
        Ok(zone_spf::qmdb::read_proof(&self.journal.history, key)?)
    }

    pub(crate) fn rewind(&mut self, height: u64) -> eyre::Result<()> {
        eyre::ensure!(height <= self.height(), "cannot rewind ahead of head");
        let mut next = initial_journal()?;
        for block in self.journal.blocks.iter().take(height as usize) {
            next = Self::execute(
                &self.config,
                &next,
                block.witness.zone_blocks[0].transactions.clone(),
            )?;
        }
        self.save(&next)?;
        self.journal = next;
        Ok(())
    }

    pub(crate) fn config(&self) -> &SpfConfig {
        &self.config
    }

    pub(crate) fn tempo_witness(&self) -> TempoStateWitness {
        TempoStateWitness {
            initial_tempo_header_rlp: alloy_rlp::encode(&self.journal.tempo_header).into(),
            node_pool: Vec::new(),
        }
    }

    fn execute(
        config: &SpfConfig,
        journal: &Journal,
        transactions: Vec<Bytes>,
    ) -> eyre::Result<Journal> {
        let parent = journal
            .blocks
            .last()
            .map_or(&journal.genesis_header, |block| &block.executed.header);
        let mut tempo_header = journal.tempo_header.clone();
        tempo_header.inner.parent_hash = tempo_header.hash_slow();
        tempo_header.inner.number += 1;
        tempo_header.inner.timestamp = parent.timestamp() + 1;
        let number = parent.number() + 1;
        let witness = BatchWitness {
            public_inputs: PublicInputs {
                parent_chain_id: PARENT_CHAIN_ID,
                zone_id: ZONE_ID,
                tempo_block_number: tempo_header.number(),
                anchor_block_number: tempo_header.number(),
                anchor_block_hash: tempo_header.hash_slow(),
                expected_withdrawal_batch_index: number,
            },
            parent_header: parent.clone(),
            zone_blocks: vec![ZoneBlock {
                number,
                parent_hash: parent.hash_slow(),
                timestamp: tempo_header.timestamp(),
                timestamp_millis_part: 0,
                beneficiary: Address::ZERO,
                tempo_import: TempoImport::Full {
                    header_rlp: alloy_rlp::encode(&tempo_header).into(),
                    deposits: Vec::new(),
                    decryptions: Vec::new(),
                    enabled_tokens: Vec::new(),
                },
                finalize_withdrawal_batch_count: Some(U256::ZERO),
                finalize_withdrawal_batch_encrypted_senders: Vec::new(),
                transactions,
            }],
            zone_state_witness: journal.state_witness.clone(),
            tempo_state_witness: TempoStateWitness {
                initial_tempo_header_rlp: alloy_rlp::encode(&journal.tempo_header).into(),
                node_pool: Vec::new(),
            },
            tempo_ancestry_headers: Vec::new(),
        };
        let input = witness.clone();
        let replay = execute_qmdb_zone_batch(config, witness, journal.history.clone())?;
        let mut next = journal.clone();
        next.history = replay.history;
        next.state_witness = replay.state_witness;
        next.tempo_header = tempo_header;
        next.blocks.push(StoredBlock {
            witness: input,
            executed: replay.blocks.into_iter().next().unwrap(),
            output: replay.output,
        });
        Ok(next)
    }

    fn save(&self, journal: &Journal) -> eyre::Result<()> {
        let path = self.datadir.join("chain.json.next");
        let mut file = File::create(&path)?;
        file.write_all(&serde_json::to_vec(journal)?)?;
        file.sync_all()?;
        fs::rename(path, self.datadir.join("chain.json"))?;
        File::open(&self.datadir)?.sync_all()?;
        Ok(())
    }
}

fn initial_journal() -> eyre::Result<Journal> {
    let mut genesis: Genesis = serde_json::from_str(include_str!(
        "../../../crates/node/assets/zone-dev-genesis.json"
    ))?;
    genesis.config.chain_id = zone_chain_id(PARENT_CHAIN_ID, ZONE_ID)?;
    let mut tempo_header = TempoHeader::default();
    tempo_header.inner.state_root = EMPTY_ROOT_HASH;
    let storage = genesis
        .alloc
        .get_mut(&TEMPO_STATE_ADDRESS)
        .unwrap()
        .storage
        .get_or_insert_default();
    storage.insert(
        tempo_state::slots::TEMPO_BLOCK_HASH.into(),
        tempo_header.hash_slow(),
    );
    storage.insert(tempo_state::slots::TEMPO_BLOCK_NUMBER.into(), B256::ZERO);
    genesis
        .alloc
        .get_mut(&PATH_USD_ADDRESS)
        .unwrap()
        .storage
        .get_or_insert_default()
        .insert(
            DEV_ACCOUNT.mapping_slot(tip20_slots::BALANCES).into(),
            U256::from(1_000_000_000_000_u64).into(),
        );
    let config = SpfConfig::new(Arc::new(ZoneChainSpec::from_genesis(genesis.clone())?));
    let mut values = BTreeMap::new();
    let mut bytecodes = BTreeMap::new();
    for (address, account) in &genesis.alloc {
        let code_hash = account.code_hash().unwrap_or(KECCAK_EMPTY);
        if let Some(code) = &account.code {
            bytecodes.insert(keccak256(code), code.clone());
        }
        values.insert(
            QmdbKey::account(*address),
            Bytes::from(alloy_rlp::encode(TrieAccount {
                nonce: account.nonce.unwrap_or_default(),
                balance: account.balance,
                storage_root: EMPTY_ROOT_HASH,
                code_hash,
            })),
        );
        for (slot, value) in account
            .storage_slots()
            .filter(|(_, value)| !value.is_zero())
        {
            values.insert(
                QmdbKey::storage(*address, slot.into()),
                Bytes::copy_from_slice(&value.to_be_bytes::<32>()),
            );
        }
    }
    let history = QmdbStateWitness {
        batches: vec![
            values
                .into_iter()
                .map(|(key, value)| QmdbMutation {
                    key,
                    value: Some(value),
                })
                .collect(),
        ],
    };
    let mut genesis_header = config.chain_spec().genesis_header().clone();
    genesis_header.inner.state_root = state_root(&history)?;
    Ok(Journal {
        version: 1,
        genesis,
        genesis_header,
        history,
        state_witness: ZoneStateWitness {
            node_pool: Vec::new(),
            bytecodes: bytecodes.into_values().collect(),
        },
        tempo_header,
        blocks: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction as _, TxLegacy, TxReceipt as _};
    use alloy_eips::eip2718::Encodable2718 as _;
    use alloy_primitives::TxKind;
    use alloy_signer::SignerSync as _;
    use alloy_signer_local::{MnemonicBuilder, coins_bip39::English};

    fn signed_transaction(node: &Node, nonce: u64, to: TxKind, data: Bytes) -> Bytes {
        let signer = MnemonicBuilder::<English>::default()
            .phrase("test test test test test test test test test test test junk")
            .build()
            .unwrap();
        assert_eq!(signer.address(), DEV_ACCOUNT);
        let transaction = TxLegacy {
            chain_id: Some(node.chain_id()),
            nonce,
            gas_price: node.head().base_fee_per_gas().unwrap() as u128,
            gas_limit: 500_000,
            to,
            value: U256::ZERO,
            input: data,
        };
        let signature = signer
            .sign_hash_sync(&transaction.signature_hash())
            .unwrap();
        tempo_primitives::TempoTxEnvelope::from(transaction.into_signed(signature))
            .encoded_2718()
            .into()
    }

    #[test]
    fn signed_token_approvals_and_storage_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let mut node = Node::open(directory.path().into()).unwrap();
        let init_code = "6007600c60003960076000f360003560005500"
            .parse::<Bytes>()
            .unwrap();
        let deploy = signed_transaction(&node, 0, TxKind::Create, init_code);
        assert!(node.mine(vec![deploy]).is_err());
        assert_eq!(node.height(), 0);
        let recipient = address!("000000000000000000000000000000000000b001");
        let slot = recipient.mapping_slot(DEV_ACCOUNT.mapping_slot(tip20_slots::ALLOWANCES));
        assert_eq!(
            node.code(PATH_USD_ADDRESS).unwrap(),
            "ef".parse::<Bytes>().unwrap()
        );
        let mut data = "095ea7b3".parse::<Bytes>().unwrap().to_vec();
        data.extend_from_slice(recipient.into_word().as_slice());
        data.extend_from_slice(&U256::from(42).to_be_bytes::<32>());
        let call = signed_transaction(
            &node,
            0,
            TxKind::Call(PATH_USD_ADDRESS),
            data.clone().into(),
        );
        node.mine(vec![call.clone()]).unwrap();
        assert_eq!(node.nonce(DEV_ACCOUNT).unwrap(), 1);
        assert!(
            node.block(1).unwrap().receipts[1].status(),
            "{:?}",
            node.block(1).unwrap().user_execution_errors
        );
        assert_eq!(
            node.storage(PATH_USD_ADDRESS, slot).unwrap(),
            U256::from(42)
        );
        let head = node.head().clone();
        // Rejected transactions must not advance the state, mock L1, or disk journal.
        assert!(node.mine(vec![call]).is_err());
        assert_eq!(node.head(), &head);
        node.prove(1).unwrap();
        drop(node);
        let mut node = Node::open(directory.path().into()).unwrap();
        assert_eq!(node.head(), &head);
        assert_eq!(
            node.storage(PATH_USD_ADDRESS, slot).unwrap(),
            U256::from(42)
        );
        let call = signed_transaction(&node, 1, TxKind::Call(PATH_USD_ADDRESS), data.into());
        node.mine(vec![call]).unwrap();
        assert_eq!(
            node.storage(PATH_USD_ADDRESS, slot).unwrap(),
            U256::from(42)
        );
        node.prove(2).unwrap();
        node.rewind(0).unwrap();
        assert_eq!(node.nonce(DEV_ACCOUNT).unwrap(), 0);
        assert_eq!(node.storage(PATH_USD_ADDRESS, slot).unwrap(), U256::ZERO);
    }

    #[test]
    fn mines_proves_restarts_and_rewinds() {
        let directory = tempfile::tempdir().unwrap();
        let mut node = Node::open(directory.path().into()).unwrap();
        let genesis_root = node.head().state_root();
        node.mine(Vec::new()).unwrap();
        node.mine(Vec::new()).unwrap();
        assert_ne!(node.head().state_root(), genesis_root);
        let head = node.head().clone();
        assert_eq!(
            node.prove(2).unwrap().block_transition.nextBlockHash,
            head.hash_slow()
        );
        assert!(Node::open(directory.path().into()).is_err());
        drop(node);
        let mut node = Node::open(directory.path().into()).unwrap();
        assert_eq!(node.head(), &head);
        node.rewind(1).unwrap();
        node.mine(Vec::new()).unwrap();
        assert_eq!(node.head(), &head);
    }

    #[test]
    fn rejects_corrupt_persisted_state() {
        let directory = tempfile::tempdir().unwrap();
        let node = Node::open(directory.path().into()).unwrap();
        let mut journal = node.journal.clone();
        journal.genesis_header.inner.state_root = B256::ZERO;
        node.save(&journal).unwrap();
        drop(node);
        assert!(Node::open(directory.path().into()).is_err());
    }
}
