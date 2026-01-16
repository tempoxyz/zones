//! Transaction execution for Privacy Zone using tempo-evm.
//!
//! Based on reth-exex-examples/rollup pattern.

use crate::db::Database;
use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_primitives::U256;
use reth_revm::{
    DatabaseCommit, State,
    context::{
        BlockEnv, Transaction,
        result::{ExecutionResult, ResultAndState},
    },
    db::BundleState,
    db::states::bundle_state::BundleRetention,
};
use reth_tracing::tracing::debug;
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_evm::{TempoBlockEnv, TempoEvmFactory, TempoHaltReason};
use tempo_revm::TempoTxEnv;

/// Execute a batch of transactions and return the bundle state.
pub fn execute_transactions(
    db: &mut Database,
    transactions: Vec<TempoTxEnv>,
    block_number: u64,
    timestamp: u64,
    gas_limit: u64,
) -> eyre::Result<(BundleState, Vec<ExecutionResult<TempoHaltReason>>)> {
    let env: EvmEnv<TempoHardfork, TempoBlockEnv> = EvmEnv {
        cfg_env: Default::default(),
        block_env: TempoBlockEnv {
            inner: BlockEnv {
                number: U256::from(block_number),
                timestamp: U256::from(timestamp),
                gas_limit,
                ..Default::default()
            },
            timestamp_millis_part: 0,
        },
    };

    let factory = TempoEvmFactory::default();
    let state = State::builder()
        .with_database(db)
        .with_bundle_update()
        .build();
    let mut evm = factory.create_evm(state, env);

    let mut results = Vec::with_capacity(transactions.len());
    let mut cumulative_gas_used = 0u64;

    for tx in transactions {
        let block_available_gas = gas_limit.saturating_sub(cumulative_gas_used);
        if Transaction::gas_limit(&tx) > block_available_gas {
            debug!(
                tx_gas = Transaction::gas_limit(&tx),
                available = block_available_gas,
                "Skipping transaction: exceeds available gas"
            );
            continue;
        }

        let result: Result<ResultAndState<_>, _> = evm.transact(tx.clone());
        match result {
            Ok(ResultAndState { result, state }) => {
                debug!(?result, "Executed transaction");
                evm.db_mut().commit(state);
                cumulative_gas_used += result.gas_used();
                results.push(result);
            }
            Err(err) => {
                #[cfg(test)]
                eprintln!("Transaction failed: {err}");
                debug!(%err, "Transaction failed, skipping");
                continue;
            }
        }
    }

    evm.db_mut().merge_transitions(BundleRetention::Reverts);
    let bundle = evm.db_mut().take_bundle();

    Ok((bundle, results))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, TxKind};
    use reth_revm::context::TxEnv;
    use rusqlite::Connection;

    #[test]
    fn test_execute_empty_transactions() {
        let mut db = Database::new(Connection::open_in_memory().unwrap()).unwrap();

        let (_bundle, results) =
            execute_transactions(&mut db, vec![], 1, 1000, 30_000_000).unwrap();

        assert!(results.is_empty());
    }

    #[test]
    fn test_execute_simple_transfer() {
        let mut db = Database::new(Connection::open_in_memory().unwrap()).unwrap();

        // Fund sender with enough balance for transfer + gas
        let sender = Address::repeat_byte(0x01);
        db.upsert_account(sender, |_| {
            Ok(reth_revm::state::AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                nonce: 0,
                ..Default::default()
            })
        })
        .unwrap();

        let tx = TempoTxEnv {
            inner: TxEnv {
                caller: sender,
                gas_limit: 21000,
                gas_price: 0,
                kind: TxKind::Call(Address::repeat_byte(0x02)),
                value: U256::from(1000),
                nonce: 0,
                ..Default::default()
            },
            is_system_tx: true, // System tx bypasses fee validation
            ..Default::default()
        };

        let (_bundle, results) =
            execute_transactions(&mut db, vec![tx], 1, 1000, 30_000_000).unwrap();

        // Transaction should succeed (simple ETH transfer)
        assert_eq!(results.len(), 1, "Expected 1 result, got {}", results.len());
        assert!(results[0].is_success(), "Transaction should succeed");
    }
}
