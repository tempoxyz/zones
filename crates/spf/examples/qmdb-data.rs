//! Import complete MPT state or measure an explicitly synthetic QMDB witness.

use std::{collections::BTreeMap, error::Error, fs, sync::Arc, time::Instant};

use alloy_primitives::{Address, B256, Bytes, U256};
use reth_trie_common::TrieAccount;
use serde::{Deserialize, Serialize};
use zone_spf::{
    BatchWitness, SpfConfig,
    qmdb::{
        QmdbKey, QmdbMutation, QmdbStateWitness, import_mpt_snapshot, read_proof, state_root,
        verify_read_proof,
    },
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    state_root: B256,
    state: Vec<Bytes>,
    read_keys: Vec<QmdbKey>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    source: String,
    qmdb_root: B256,
    history_json_bytes: usize,
    imported_rows: usize,
    root_build_micros: u128,
    proofs: Vec<ProofSize>,
    witness: QmdbStateWitness,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofSize {
    key: QmdbKey,
    present: bool,
    proof_bytes: usize,
    value_bytes: usize,
    verified: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--prove-qmdb")
    {
        if arguments.len() != 4 {
            return Err("expected --prove-qmdb GENESIS_JSON BATCH_JSON HISTORY_JSON".into());
        }
        let genesis = serde_json::from_slice(&fs::read(&arguments[1])?)?;
        let batch: BatchWitness = serde_json::from_slice(&fs::read(&arguments[2])?)?;
        let history = serde_json::from_slice(&fs::read(&arguments[3])?)?;
        let config = SpfConfig::new(Arc::new(zone_chainspec::ZoneChainSpec::from_genesis(
            genesis,
        )?));
        let output = zone_spf::qmdb::prove_qmdb_zone_batch(&config, batch, history)?;
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    let input = std::env::args()
        .nth(1)
        .ok_or("expected --demo or a complete MPT snapshot JSON path")?;
    let (source, witness, keys) = if input == "--demo" {
        let (witness, keys) = demo();
        (
            "synthetic: 256 accounts, 8 slots each; not a live Zone witness".into(),
            witness,
            keys,
        )
    } else {
        let snapshot: Snapshot = serde_json::from_slice(&fs::read(&input)?)?;
        (
            format!("complete MPT snapshot: {input}"),
            import_mpt_snapshot(snapshot.state_root, &snapshot.state)?,
            snapshot.read_keys,
        )
    };
    let started = Instant::now();
    let qmdb_root = state_root(&witness)?;
    let root_build_micros = started.elapsed().as_micros();
    let proofs = keys
        .into_iter()
        .map(|key| {
            let proof = read_proof(&witness, key)?;
            let verified = verify_read_proof(qmdb_root, key, &proof);
            if !verified {
                return Err("generated read proof failed verification".into());
            }
            Ok(ProofSize {
                key,
                present: proof.value.is_some(),
                proof_bytes: proof.proof.len(),
                value_bytes: proof.value.as_ref().map_or(0, |value| value.len()),
                verified,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    let report = Report {
        source,
        qmdb_root,
        history_json_bytes: serde_json::to_vec(&witness)?.len(),
        imported_rows: witness.batches.first().map_or(0, Vec::len),
        root_build_micros,
        proofs,
        witness,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn demo() -> (QmdbStateWitness, Vec<QmdbKey>) {
    let mut values = BTreeMap::new();
    for index in 1_u64..=256 {
        let address = Address::from_word(B256::from(U256::from(index)));
        let account = TrieAccount {
            nonce: index,
            balance: U256::from(index * 100),
            ..Default::default()
        };
        values.insert(
            QmdbKey::account(address),
            Bytes::from(alloy_rlp::encode(account)),
        );
        for slot in 0_u64..8 {
            values.insert(
                QmdbKey::storage(address, U256::from(slot)),
                Bytes::copy_from_slice(&U256::from(index + slot).to_be_bytes::<32>()),
            );
        }
    }
    let address = Address::from_word(B256::from(U256::from(128)));
    let keys = vec![
        QmdbKey::account(address),
        QmdbKey::storage(address, U256::from(3)),
        QmdbKey::storage(address, U256::from(9)),
        QmdbKey::account(Address::ZERO),
    ];
    (
        QmdbStateWitness {
            batches: vec![
                values
                    .into_iter()
                    .map(|(key, value)| QmdbMutation {
                        key,
                        value: Some(value),
                    })
                    .collect(),
            ],
        },
        keys,
    )
}
