//! EVM execution for Privacy Zone L2.
//!
//! Based on reth-exex-examples/rollup pattern.

use alloy_consensus::{Header, Transaction};
use alloy_eips::eip1559::INITIAL_BASE_FEE;
use alloy_primitives::{Address, U256};
use reth_chainspec::{ChainSpec, EthereumHardfork};
use reth_evm::{ConfigureEvm, Evm, precompiles::PrecompilesMap};
use reth_execution_errors::BlockValidationError;
use reth_node_ethereum::{EthEvmConfig, evm::EthEvm};
use reth_primitives::{Block, BlockBody, Receipt, Recovered, RecoveredBlock, TransactionSigned};
use reth_primitives_traits::Block as _;
use reth_revm::{
    DatabaseCommit, State,
    context::result::{EVMError, ExecutionResult, ResultAndState},
    db::{BundleState, StateBuilder, states::bundle_state::BundleRetention},
    inspector::NoOpInspector,
};
use std::sync::Arc;
use tracing::debug;

use crate::db::L2Database;

/// Execute an L2 block with the given transactions.
///
/// Returns the executed block, bundle state, receipts, and execution results.
pub fn execute_block(
    db: &mut L2Database,
    chain_spec: Arc<ChainSpec>,
    parent_header: Option<&Header>,
    block_number: u64,
    timestamp: u64,
    gas_limit: u64,
    transactions: Vec<Recovered<TransactionSigned>>,
) -> eyre::Result<(
    RecoveredBlock<Block>,
    BundleState,
    Vec<Receipt>,
    Vec<ExecutionResult>,
)> {
    // Construct header
    let header = construct_header(
        chain_spec.clone(),
        parent_header,
        block_number,
        timestamp,
        gas_limit,
    )?;

    // Configure EVM
    let evm_config = EthEvmConfig::new(chain_spec);
    let state = StateBuilder::new_with_database(db)
        .with_bundle_update()
        .build();
    let mut evm = evm_config.evm_for_block(state, &header)?;

    // Execute transactions
    let (executed_txs, receipts, results) = execute_transactions(&mut evm, &header, transactions)?;

    // Construct block and recover senders
    let block = Block {
        header,
        body: BlockBody {
            transactions: executed_txs,
            ..Default::default()
        },
    }
    .try_into_recovered()?;

    let bundle = evm.db_mut().take_bundle();

    Ok((block, bundle, receipts, results))
}

/// Construct a block header.
fn construct_header(
    chain_spec: Arc<ChainSpec>,
    parent: Option<&Header>,
    block_number: u64,
    timestamp: u64,
    gas_limit: u64,
) -> eyre::Result<Header> {
    // Calculate base fee per gas for EIP-1559 transactions
    let base_fee_per_gas = if chain_spec
        .fork(EthereumHardfork::London)
        .transitions_at_block(block_number)
    {
        INITIAL_BASE_FEE
    } else if let Some(parent) = parent {
        parent
            .next_block_base_fee(chain_spec.base_fee_params_at_timestamp(timestamp))
            .ok_or_else(|| eyre::eyre!("failed to calculate base fee"))?
    } else {
        INITIAL_BASE_FEE
    };

    Ok(Header {
        parent_hash: parent.map(|p| p.hash_slow()).unwrap_or_default(),
        number: block_number,
        gas_limit,
        timestamp,
        base_fee_per_gas: Some(base_fee_per_gas),
        ..Default::default()
    })
}

/// Execute transactions and return executed transactions, receipts, and results.
fn execute_transactions<DB: reth_evm::Database>(
    evm: &mut EthEvm<State<DB>, NoOpInspector, PrecompilesMap>,
    header: &Header,
    transactions: Vec<Recovered<TransactionSigned>>,
) -> eyre::Result<(Vec<TransactionSigned>, Vec<Receipt>, Vec<ExecutionResult>)>
where
    DB::Error: Send,
{
    let mut receipts = Vec::with_capacity(transactions.len());
    let mut executed_txs = Vec::with_capacity(transactions.len());
    let mut results = Vec::with_capacity(transactions.len());

    if transactions.is_empty() {
        return Ok((executed_txs, receipts, results));
    }

    let mut cumulative_gas_used = 0;

    for transaction in transactions {
        // Check gas limit
        let block_available_gas = header.gas_limit - cumulative_gas_used;
        if transaction.gas_limit() > block_available_gas {
            return Err(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: transaction.gas_limit(),
                    block_available_gas,
                }
                .into(),
            );
        }

        // Execute transaction
        let ResultAndState { result, state } = match evm.transact(&transaction) {
            Ok(result) => result,
            Err(err) => {
                match err {
                    EVMError::Transaction(err) => {
                        // Skip invalid transactions
                        debug!(%err, ?transaction, "Skipping invalid transaction");
                        continue;
                    }
                    _ => {
                        eyre::bail!("EVM error: {:?}", err)
                    }
                }
            }
        };

        debug!(?transaction, ?result, "Executed transaction");

        evm.db_mut().commit(state);

        cumulative_gas_used += result.gas_used();

        receipts.push(Receipt {
            tx_type: transaction.tx_type(),
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.logs().to_vec(),
            ..Default::default()
        });

        executed_txs.push(transaction.into_inner());
        results.push(result);
    }

    evm.db_mut().merge_transitions(BundleRetention::Reverts);

    Ok((executed_txs, receipts, results))
}

/// Process a deposit by crediting the recipient's balance.
///
/// This creates a synthetic transaction that credits ETH to the recipient.
pub fn process_deposit(db: &mut L2Database, to: Address, amount: U256) -> eyre::Result<()> {
    db.credit_balance(to, amount)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::MAINNET;

    #[test]
    fn test_construct_header_genesis() {
        let header = construct_header(MAINNET.clone(), None, 0, 1000, 30_000_000).unwrap();

        assert_eq!(header.number, 0);
        assert_eq!(header.timestamp, 1000);
        assert_eq!(header.gas_limit, 30_000_000);
        assert!(header.base_fee_per_gas.is_some());
    }

    #[test]
    fn test_execute_empty_block() {
        let mut db = L2Database::new_in_memory().unwrap();

        let (block, _bundle, receipts, results) =
            execute_block(&mut db, MAINNET.clone(), None, 0, 1000, 30_000_000, vec![]).unwrap();

        assert_eq!(block.header().number, 0);
        assert!(receipts.is_empty());
        assert!(results.is_empty());
    }
}
