//! Loopback-only Ethereum-compatible test RPC and explicit native QMDB proof endpoints.

use std::sync::{Arc, Mutex};

use alloy_consensus::{
    BlockHeader as _, Sealable as _, Transaction as _, TxReceipt as _,
    transaction::{SignerRecoverable as _, TxHashRef as _},
};
use alloy_eips::eip2718::Decodable2718 as _;
use alloy_evm::{Evm as _, EvmFactory as _};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use jsonrpsee::{RpcModule, types::ErrorObjectOwned};
use reth_evm::ConfigureEvm as _;
use revm::{context::TxEnv, database::State};
use serde_json::{Value, json};
use tempo_primitives::TempoTxEnvelope;
use tempo_revm::TempoTxEnv;
use zone_spf::TempoWitnessDatabase;

use crate::node::{DEV_ACCOUNT, Node};

const METHODS: &[&str] = &[
    "web3_clientVersion",
    "net_version",
    "eth_chainId",
    "eth_blockNumber",
    "eth_accounts",
    "eth_syncing",
    "eth_gasPrice",
    "eth_maxPriorityFeePerGas",
    "eth_getBalance",
    "eth_getCode",
    "eth_getTransactionCount",
    "eth_getStorageAt",
    "eth_sendRawTransaction",
    "eth_getBlockByNumber",
    "eth_getBlockByHash",
    "eth_getTransactionReceipt",
    "eth_getTransactionByHash",
    "eth_getRawTransactionByHash",
    "eth_call",
    "eth_estimateGas",
    "evm_mine",
    "debug_qmdbExecutionWitness",
    "qmdb_proveBlock",
    "qmdb_exportCheckpoint",
    "qmdb_rewindTo",
    "qmdb_status",
    "qmdb_getProof",
];

pub(crate) fn module(node: Arc<Mutex<Node>>) -> eyre::Result<RpcModule<Arc<Mutex<Node>>>> {
    let mut module = RpcModule::new(node);
    for method in METHODS.iter().copied() {
        module.register_async_method(method, move |params, context, _| async move {
            let params = if params.as_str().is_none() {
                Vec::new()
            } else {
                params.parse::<Vec<Value>>()?
            };
            tokio::task::spawn_blocking(move || {
                let mut node = context
                    .lock()
                    .map_err(|_| rpc_error("node lock poisoned"))?;
                dispatch(&mut node, method, &params).map_err(|error| rpc_error(error.to_string()))
            })
            .await
            .map_err(|error| rpc_error(error.to_string()))?
        })?;
    }
    Ok(module)
}

fn rpc_error(message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-32000, message.into(), None::<()>)
}

fn parameter<T: serde::de::DeserializeOwned>(params: &[Value], index: usize) -> eyre::Result<T> {
    Ok(serde_json::from_value(
        params
            .get(index)
            .ok_or_else(|| eyre::eyre!("missing parameter {index}"))?
            .clone(),
    )?)
}

fn number(node: &Node, value: &Value) -> eyre::Result<u64> {
    match value.as_str() {
        Some("latest" | "pending" | "safe" | "finalized") => Ok(node.height()),
        Some("earliest") => Ok(0),
        Some(value) => Ok(value.parse::<U256>()?.try_into()?),
        None => value
            .as_u64()
            .ok_or_else(|| eyre::eyre!("invalid block number")),
    }
}

fn current_state(node: &Node, params: &[Value], index: usize) -> eyre::Result<()> {
    if let Some(value) = params.get(index) {
        eyre::ensure!(
            number(node, value)? == node.height(),
            "historical state queries are not supported; use latest"
        );
    }
    Ok(())
}

fn dispatch(node: &mut Node, method: &str, params: &[Value]) -> eyre::Result<Value> {
    Ok(match method {
        "web3_clientVersion" => json!("tempo-zone-qmdb/test-mock-l1"),
        "net_version" => json!(node.chain_id().to_string()),
        "eth_chainId" => json!(format!("0x{:x}", node.chain_id())),
        "eth_blockNumber" => json!(format!("0x{:x}", node.height())),
        "eth_accounts" => json!([DEV_ACCOUNT]),
        "eth_syncing" => json!(false),
        "eth_gasPrice" => json!(format!(
            "0x{:x}",
            node.head().base_fee_per_gas().unwrap_or_default()
        )),
        "eth_maxPriorityFeePerGas" => json!("0x0"),
        "eth_getBalance" => {
            current_state(node, params, 1)?;
            json!(node.balance(parameter(params, 0)?)?)
        }
        "eth_getTransactionCount" => {
            current_state(node, params, 1)?;
            json!(format!("0x{:x}", node.nonce(parameter(params, 0)?)?))
        }
        "eth_getCode" => {
            current_state(node, params, 1)?;
            json!(node.code(parameter(params, 0)?)?)
        }
        "eth_getStorageAt" => {
            current_state(node, params, 2)?;
            let slot: U256 = parameter(params, 1)?;
            json!(B256::from(node.storage(parameter(params, 0)?, slot)?))
        }
        "eth_sendRawTransaction" => {
            let raw: Bytes = parameter(params, 0)?;
            let mut bytes = raw.as_ref();
            let transaction = TempoTxEnvelope::decode_2718(&mut bytes)?;
            eyre::ensure!(bytes.is_empty(), "trailing transaction bytes");
            transaction.recover_signer()?;
            let hash = *transaction.tx_hash();
            node.mine(vec![raw])?;
            json!(hash)
        }
        "eth_getBlockByNumber" => block(
            node,
            number(
                node,
                params.first().ok_or_else(|| eyre::eyre!("missing block"))?,
            )?,
            params.get(1).and_then(Value::as_bool).unwrap_or(false),
        )?,
        "eth_getBlockByHash" => {
            let hash: B256 = parameter(params, 0)?;
            let found = (0..=node.height()).find(|number| {
                header(node, *number).is_some_and(|header| header.hash_slow() == hash)
            });
            match found {
                Some(number) => block(
                    node,
                    number,
                    params.get(1).and_then(Value::as_bool).unwrap_or(false),
                )?,
                None => Value::Null,
            }
        }
        "eth_getTransactionReceipt" | "eth_getTransactionByHash" => {
            let hash: B256 = parameter(params, 0)?;
            transaction(node, hash, method == "eth_getTransactionReceipt")?
        }
        "eth_getRawTransactionByHash" => {
            let hash: B256 = parameter(params, 0)?;
            let raw = (1..=node.height())
                .filter_map(|number| node.block(number))
                .flat_map(|block| &block.transactions)
                .find(|transaction| *transaction.tx_hash() == hash)
                .map(|transaction| {
                    Bytes::from(alloy_eips::eip2718::Encodable2718::encoded_2718(
                        transaction,
                    ))
                });
            json!(raw)
        }
        "eth_call" | "eth_estimateGas" => {
            current_state(node, params, 1)?;
            eyre::ensure!(
                params.len() <= 2,
                "state and block overrides are not supported"
            );
            simulate(node, parameter(params, 0)?, method == "eth_estimateGas")?
        }
        "evm_mine" => json!(node.mine(Vec::new())?),
        "debug_qmdbExecutionWitness" => json!(node.proof_input(number(
            node,
            params.first().ok_or_else(|| eyre::eyre!("missing block"))?
        )?)?),
        "qmdb_proveBlock" => json!({"attested": false, "kind": "native-spf-replay",
            "output": node.prove(number(node, params.first().ok_or_else(|| eyre::eyre!("missing block"))?)?)?}),
        "qmdb_exportCheckpoint" => node.checkpoint(),
        "qmdb_rewindTo" => {
            node.rewind(number(
                node,
                params
                    .first()
                    .ok_or_else(|| eyre::eyre!("missing height"))?,
            )?)?;
            json!(true)
        }
        "qmdb_status" => json!({"stateBackend": "qmdb-current-ordered", "height": node.height(),
            "stateRoot": node.head().state_root(), "mockL1": true, "attested": false,
            "settlement": false, "witnessMode": "full-history", "devAccount": DEV_ACCOUNT}),
        "qmdb_getProof" => {
            current_state(node, params, 2)?;
            let address: Address = parameter(params, 0)?;
            let slots: Vec<U256> = parameter(params, 1)?;
            eyre::ensure!(slots.len() <= 32, "at most 32 storage proofs per request");
            let storage = slots
                .into_iter()
                .map(|slot| {
                    let key = zone_spf::QmdbKey::storage(address, slot);
                    Ok(json!({"slot": slot, "key": key, "proof": node.read_proof(key)?}))
                })
                .collect::<eyre::Result<Vec<_>>>()?;
            let key = zone_spf::QmdbKey::account(address);
            json!({"stateRoot": node.head().state_root(), "accountKey": key,
                "accountProof": node.read_proof(key)?, "storageProofs": storage})
        }
        _ => eyre::bail!("method not supported"),
    })
}

fn header(node: &Node, number: u64) -> Option<&tempo_primitives::TempoHeader> {
    if number == 0 {
        Some(node.genesis_header())
    } else {
        node.block(number).map(|block| &block.header)
    }
}

fn block(node: &Node, number: u64, full: bool) -> eyre::Result<Value> {
    let Some(header) = header(node, number) else {
        return Ok(Value::Null);
    };
    let mut value = serde_json::to_value(header)?;
    let fields = value.as_object_mut().unwrap();
    fields.insert("hash".into(), json!(header.hash_slow()));
    fields.insert("totalDifficulty".into(), json!("0x0"));
    fields.insert("uncles".into(), json!([]));
    let body = alloy_consensus::BlockBody {
        transactions: node
            .block(number)
            .map_or_else(Vec::new, |block| block.transactions.clone()),
        ommers: Vec::new(),
        withdrawals: header
            .withdrawals_root()
            .map(|_| alloy_eips::eip4895::Withdrawals::default()),
    };
    let consensus_block = alloy_consensus::Block::new(header.clone(), body);
    fields.insert(
        "size".into(),
        json!(format!("0x{:x}", alloy_rlp::encode(&consensus_block).len())),
    );
    let transactions = if let Some(block) = node.block(number) {
        block
            .transactions
            .iter()
            .enumerate()
            .map(|(index, transaction)| {
                if full {
                    transaction_value(transaction, header, index)
                } else {
                    Ok(json!(transaction.tx_hash()))
                }
            })
            .collect::<eyre::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    fields.insert("transactions".into(), json!(transactions));
    Ok(value)
}

fn transaction_value(
    transaction: &TempoTxEnvelope,
    header: &tempo_primitives::TempoHeader,
    index: usize,
) -> eyre::Result<Value> {
    let mut value = serde_json::to_value(transaction)?;
    let fields = value.as_object_mut().unwrap();
    fields.insert("hash".into(), json!(transaction.tx_hash()));
    fields.insert(
        "from".into(),
        json!(transaction.recover_signer().unwrap_or(Address::ZERO)),
    );
    fields.insert("blockHash".into(), json!(header.hash_slow()));
    fields.insert(
        "blockNumber".into(),
        json!(format!("0x{:x}", header.number())),
    );
    fields.insert("transactionIndex".into(), json!(format!("0x{index:x}")));
    Ok(value)
}

fn transaction(node: &Node, hash: B256, receipt: bool) -> eyre::Result<Value> {
    for number in 1..=node.height() {
        let block = node.block(number).unwrap();
        if let Some((index, transaction)) = block
            .transactions
            .iter()
            .enumerate()
            .find(|(_, transaction)| *transaction.tx_hash() == hash)
        {
            if !receipt {
                return transaction_value(transaction, &block.header, index);
            }
            let result = &block.receipts[index];
            let previous_gas = index
                .checked_sub(1)
                .map_or(0, |previous| block.receipts[previous].cumulative_gas_used());
            let from = transaction.recover_signer().unwrap_or(Address::ZERO);
            let mut value = serde_json::to_value(result)?;
            let fields = value.as_object_mut().unwrap();
            fields.insert("transactionHash".into(), json!(hash));
            fields.insert("transactionIndex".into(), json!(format!("0x{index:x}")));
            fields.insert("blockHash".into(), json!(block.header.hash_slow()));
            fields.insert("blockNumber".into(), json!(format!("0x{number:x}")));
            fields.insert("from".into(), json!(from));
            fields.insert("to".into(), json!(transaction.to()));
            fields.insert(
                "gasUsed".into(),
                json!(format!(
                    "0x{:x}",
                    result.cumulative_gas_used() - previous_gas
                )),
            );
            fields.insert(
                "effectiveGasPrice".into(),
                json!(format!(
                    "0x{:x}",
                    transaction.effective_gas_price(block.header.base_fee_per_gas())
                )),
            );
            fields.insert(
                "contractAddress".into(),
                json!(if transaction.is_create() && result.status() {
                    Some(from.create(transaction.nonce()))
                } else {
                    None
                }),
            );
            fields.insert("logsBloom".into(), json!(result.bloom()));
            let first_log_index = block.receipts[..index]
                .iter()
                .map(|receipt| receipt.logs().len())
                .sum::<usize>();
            let logs = result
                .logs()
                .iter()
                .enumerate()
                .map(|(offset, log)| {
                    let mut log = serde_json::to_value(log)?;
                    let fields = log.as_object_mut().unwrap();
                    fields.insert("blockHash".into(), json!(block.header.hash_slow()));
                    fields.insert("blockNumber".into(), json!(format!("0x{number:x}")));
                    fields.insert(
                        "blockTimestamp".into(),
                        json!(format!("0x{:x}", block.header.timestamp())),
                    );
                    fields.insert("transactionHash".into(), json!(hash));
                    fields.insert("transactionIndex".into(), json!(format!("0x{index:x}")));
                    fields.insert(
                        "logIndex".into(),
                        json!(format!("0x{:x}", first_log_index + offset)),
                    );
                    fields.insert("removed".into(), json!(false));
                    Ok(log)
                })
                .collect::<eyre::Result<Vec<_>>>()?;
            fields.insert("logs".into(), json!(logs));
            return Ok(value);
        }
    }
    Ok(Value::Null)
}

fn simulate(node: &Node, request: Value, estimate: bool) -> eyre::Result<Value> {
    let from: Address = request
        .get("from")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or(DEV_ACCOUNT);
    let to: Option<Address> = request
        .get("to")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?;
    let data: Bytes = request
        .get("input")
        .or_else(|| request.get("data"))
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    let value: U256 = request
        .get("value")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    let gas: U256 = request
        .get("gas")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or(U256::from(node.head().gas_limit()));
    let database = node.database()?;
    let state = State::builder().with_database(database).build();
    let tempo = TempoWitnessDatabase::from_tempo_state_witness(node.tempo_witness())?;
    let config = node.config().evm_config(tempo);
    let mut env = config.evm_env(node.head())?;
    env.cfg_env.disable_base_fee = true;
    env.cfg_env.disable_fee_charge = true;
    let mut evm = config.evm_factory().create_evm(state, env);
    let result = evm.transact_raw(TempoTxEnv {
        inner: TxEnv {
            caller: from,
            kind: to.map_or(TxKind::Create, TxKind::Call),
            data,
            value,
            gas_limit: gas.try_into()?,
            nonce: node.nonce(from)?,
            chain_id: Some(node.chain_id()),
            ..Default::default()
        },
        ..Default::default()
    })?;
    eyre::ensure!(
        result.result.is_success(),
        "simulation failed: {:?}",
        result.result
    );
    if estimate {
        Ok(json!(format!(
            "0x{:x}",
            result.result.tx_gas_used() + 20_000
        )))
    } else {
        Ok(json!(result.result.output().cloned().unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rpc_mines_and_proves() {
        let directory = tempfile::tempdir().unwrap();
        let module = module(Arc::new(Mutex::new(
            Node::open(directory.path().into()).unwrap(),
        )))
        .unwrap();
        let result: Value = module
            .call("qmdb_status", Vec::<Value>::new())
            .await
            .unwrap();
        assert_eq!(result["stateBackend"], "qmdb-current-ordered");
        let _: Value = module.call("evm_mine", Vec::<Value>::new()).await.unwrap();
        let result: Value = module.call("qmdb_proveBlock", ["0x1"]).await.unwrap();
        assert_eq!(result["attested"], false);
        assert_eq!(result["output"]["nextZoneHeight"], 1);
    }
}
