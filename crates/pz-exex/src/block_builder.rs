//! Zone block builder for Privacy Zone.
//!
//! Builds zone blocks from pending deposits and zone transactions.

use crate::{
    db::Database,
    execution::execute_transactions,
    types::{exitCall, ExitIntent, L1Cursor, PzConfig, PzState, EXIT_PRECOMPILE},
};
use alloy_primitives::B256;
use alloy_sol_types::SolCall;
use reth_revm::db::BundleState;
use reth_tracing::tracing::{debug, info};
use tempo_revm::TempoTxEnv;

/// Result of building a zone block.
#[derive(Debug)]
pub struct ZoneBlock {
    /// Zone block number.
    pub number: u64,
    /// Timestamp of the block.
    pub timestamp: u64,
    /// State root after execution.
    pub state_root: B256,
    /// Journal hash for provenance.
    pub journal_hash: B256,
    /// L1 cursor at block finalization.
    pub cursor: L1Cursor,
    /// Number of deposits processed.
    pub deposits_processed: usize,
    /// Number of transactions executed.
    pub transactions_executed: usize,
    /// Exit intents recorded in this block.
    pub exits: Vec<ExitIntent>,
    /// Bundle state changes.
    pub bundle: BundleState,
}

/// Builder for constructing zone blocks.
pub struct ZoneBlockBuilder<'a> {
    db: &'a mut Database,
    config: &'a PzConfig,
    state: PzState,
    transactions: Vec<TempoTxEnv>,
    gas_limit: u64,
}

impl<'a> ZoneBlockBuilder<'a> {
    /// Create a new block builder.
    pub fn new(db: &'a mut Database, config: &'a PzConfig, state: PzState) -> Self {
        Self {
            db,
            config,
            state,
            transactions: Vec::new(),
            gas_limit: 30_000_000, // Default gas limit
        }
    }

    /// Set the gas limit for this block.
    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Add a transaction to the block.
    pub fn add_transaction(&mut self, tx: TempoTxEnv) {
        self.transactions.push(tx);
    }

    /// Add multiple transactions to the block.
    pub fn add_transactions(&mut self, txs: impl IntoIterator<Item = TempoTxEnv>) {
        self.transactions.extend(txs);
    }

    /// Build the zone block, processing pending deposits and executing transactions.
    ///
    /// This method:
    /// 1. Converts pending deposits to system transactions
    /// 2. Executes all transactions (deposits first, then user txs)
    /// 3. Extracts exit intents from transactions
    /// 4. Updates state root and journal hash
    /// 5. Persists state changes to the database
    pub fn build(self, timestamp: u64) -> eyre::Result<ZoneBlock> {
        let ZoneBlockBuilder { db, config, mut state, transactions, gas_limit } = self;

        let block_number = state.zone_block + 1;

        // Get pending deposits
        let pending_deposits = db.get_pending_deposits()?;
        let deposits_count = pending_deposits.len();

        debug!(
            zone_id = config.zone_id,
            block = block_number,
            deposits = deposits_count,
            transactions = transactions.len(),
            "Building zone block"
        );

        // Track deposit cursors
        let mut last_deposit_cursor = None;
        for (cursor, _hash, deposit) in &pending_deposits {
            last_deposit_cursor = Some(*cursor);
            debug!(
                to = %deposit.to,
                amount = %deposit.amount,
                l1_block = cursor.block_number,
                "Including deposit in zone block"
            );
        }

        // Extract exit intents from transactions before execution
        let mut exits = Vec::new();
        let mut exit_index = 0u64;

        for tx in &transactions {
            if let Some(exit) = extract_exit_intent(tx, block_number, exit_index) {
                debug!(
                    sender = %exit.sender,
                    recipient = %exit.recipient,
                    amount = %exit.amount,
                    "Exit intent detected"
                );
                exits.push(exit);
                exit_index += 1;
            }
        }

        // Execute all transactions
        let (bundle, results) = execute_transactions(
            db,
            transactions,
            block_number,
            timestamp,
            gas_limit,
        )?;

        let txs_executed = results.len();

        // Commit bundle to database
        db.insert_block_with_bundle(block_number, bundle.clone())?;

        // Mark deposits as processed
        if let Some(cursor) = last_deposit_cursor {
            db.mark_deposits_processed(block_number, cursor)?;
        }

        // Record exit intents
        let mut exits_hash = state.exits_hash;
        for exit in &exits {
            let exit_hash = exit.hash(exits_hash);
            db.insert_exit(exit, exit_hash)?;
            exits_hash = exit_hash;
        }

        // Compute new state root (simplified - real impl would compute MPT root)
        let state_root = compute_state_root(&bundle);

        // Compute journal hash
        let block_data = encode_block_data(block_number, timestamp, &bundle);
        let journal_hash = state.next_journal_hash(&block_data);

        // Determine final cursor
        let cursor = last_deposit_cursor.unwrap_or(state.cursor);

        // Store journal entry
        db.insert_journal(block_number, journal_hash, cursor)?;

        // Update state
        state.zone_block = block_number;
        state.state_root = state_root;
        state.journal_hash = journal_hash;
        state.exits_hash = exits_hash;
        if deposits_count > 0 {
            state.processed_deposits_hash = state.deposits_hash;
        }
        db.set_zone_state(&state)?;

        info!(
            zone_id = config.zone_id,
            block = block_number,
            deposits = deposits_count,
            transactions = txs_executed,
            exits = exits.len(),
            state_root = %state_root,
            "Zone block built"
        );

        Ok(ZoneBlock {
            number: block_number,
            timestamp,
            state_root,
            journal_hash,
            cursor,
            deposits_processed: deposits_count,
            transactions_executed: txs_executed,
            exits,
            bundle,
        })
    }
}

/// Extract an exit intent from a transaction if it calls the exit precompile.
fn extract_exit_intent(tx: &TempoTxEnv, zone_block: u64, exit_index: u64) -> Option<ExitIntent> {
    use alloy_primitives::TxKind;

    // Check if tx is calling the exit precompile
    let TxKind::Call(to) = tx.inner.kind else {
        return None;
    };

    if to != EXIT_PRECOMPILE {
        return None;
    }

    // Decode the exit call
    let call = exitCall::abi_decode(&tx.inner.data).ok()?;

    Some(ExitIntent {
        sender: tx.inner.caller,
        recipient: call.recipient,
        amount: call.amount,
        zone_block,
        exit_index,
    })
}

/// Compute state root from bundle state (placeholder implementation).
/// Real implementation would compute MPT root.
fn compute_state_root(bundle: &BundleState) -> B256 {
    use alloy_primitives::keccak256;

    let mut data = Vec::new();
    for (address, account) in bundle.state() {
        data.extend_from_slice(address.as_slice());
        if let Some(info) = &account.info {
            data.extend_from_slice(&info.balance.to_be_bytes::<32>());
            data.extend_from_slice(&info.nonce.to_be_bytes());
        }
    }
    if data.is_empty() {
        B256::ZERO
    } else {
        keccak256(&data)
    }
}

/// Encode block data for journal hashing.
fn encode_block_data(number: u64, timestamp: u64, bundle: &BundleState) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&number.to_be_bytes());
    data.extend_from_slice(&timestamp.to_be_bytes());
    // Include a summary of state changes
    data.extend_from_slice(&(bundle.state().len() as u64).to_be_bytes());
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, U256};
    use reth_revm::state::AccountInfo;
    use rusqlite::Connection;

    fn test_db() -> Database {
        Database::new(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn test_config() -> PzConfig {
        PzConfig {
            zone_id: 1,
            portal_address: Address::ZERO,
            gas_token: Address::ZERO,
            sequencer: Address::ZERO,
            genesis_state_root: B256::ZERO,
        }
    }

    #[test]
    fn test_build_empty_block() {
        let mut db = test_db();
        let config = test_config();
        let state = PzState::default();

        let builder = ZoneBlockBuilder::new(&mut db, &config, state);
        let block = builder.build(1000).unwrap();

        assert_eq!(block.number, 1);
        assert_eq!(block.timestamp, 1000);
        assert_eq!(block.deposits_processed, 0);
        assert_eq!(block.transactions_executed, 0);

        // Verify state was updated
        let new_state = db.get_zone_state().unwrap();
        assert_eq!(new_state.zone_block, 1);
    }

    #[test]
    fn test_build_block_with_deposits() {
        use crate::types::Deposit;

        let mut db = test_db();
        let config = test_config();
        let state = PzState::default();

        // Queue some deposits
        let deposit1 = Deposit {
            l1_block_hash: B256::ZERO,
            l1_block_number: 100,
            l1_timestamp: 1000,
            sender: address!("1111111111111111111111111111111111111111"),
            to: address!("2222222222222222222222222222222222222222"),
            amount: U256::from(1000),
            gas_limit: 0,
            data: Default::default(),
        };
        let deposit2 = Deposit {
            l1_block_number: 100,
            amount: U256::from(2000),
            ..deposit1.clone()
        };

        db.queue_deposit(L1Cursor::new(100, 0), &deposit1, B256::repeat_byte(0x01))
            .unwrap();
        db.queue_deposit(L1Cursor::new(100, 1), &deposit2, B256::repeat_byte(0x02))
            .unwrap();

        // Credit balances (normally done in exex.rs)
        db.upsert_account(deposit1.to, |_| {
            Ok(AccountInfo {
                balance: deposit1.amount + deposit2.amount,
                ..Default::default()
            })
        })
        .unwrap();

        let builder = ZoneBlockBuilder::new(&mut db, &config, state);
        let block = builder.build(1000).unwrap();

        assert_eq!(block.number, 1);
        assert_eq!(block.deposits_processed, 2);
        assert_eq!(block.cursor, L1Cursor::new(100, 1));

        // Verify deposits are marked as processed
        let pending = db.get_pending_deposits().unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_build_sequential_blocks() {
        let mut db = test_db();
        let config = test_config();

        // Build first block
        let state = PzState::default();
        let builder = ZoneBlockBuilder::new(&mut db, &config, state);
        let block1 = builder.build(1000).unwrap();
        assert_eq!(block1.number, 1);

        // Build second block
        let state = db.get_zone_state().unwrap();
        let builder = ZoneBlockBuilder::new(&mut db, &config, state);
        let block2 = builder.build(2000).unwrap();
        assert_eq!(block2.number, 2);

        // Journal hashes should be chained
        assert_ne!(block1.journal_hash, block2.journal_hash);

        // Verify journal entries
        assert_eq!(db.get_journal_hash(1).unwrap(), Some(block1.journal_hash));
        assert_eq!(db.get_journal_hash(2).unwrap(), Some(block2.journal_hash));
    }

    #[test]
    fn test_build_block_with_exits() {
        use alloy_primitives::{Bytes, TxKind};
        use alloy_sol_types::SolCall;
        use reth_revm::context::TxEnv;

        let mut db = test_db();
        let config = test_config();
        let state = PzState::default();

        // Fund the sender
        let sender = address!("1111111111111111111111111111111111111111");
        db.upsert_account(sender, |_| {
            Ok(AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                nonce: 0,
                ..Default::default()
            })
        })
        .unwrap();

        // Create an exit transaction
        let recipient = address!("2222222222222222222222222222222222222222");
        let exit_amount = U256::from(1000);
        let exit_calldata = exitCall { recipient, amount: exit_amount }.abi_encode();

        let exit_tx = TempoTxEnv {
            inner: TxEnv {
                caller: sender,
                gas_limit: 100000,
                gas_price: 0,
                kind: TxKind::Call(EXIT_PRECOMPILE),
                data: Bytes::from(exit_calldata),
                nonce: 0,
                ..Default::default()
            },
            is_system_tx: true,
            ..Default::default()
        };

        let mut builder = ZoneBlockBuilder::new(&mut db, &config, state);
        builder.add_transaction(exit_tx);
        let block = builder.build(1000).unwrap();

        assert_eq!(block.number, 1);
        assert_eq!(block.exits.len(), 1);

        let exit = &block.exits[0];
        assert_eq!(exit.sender, sender);
        assert_eq!(exit.recipient, recipient);
        assert_eq!(exit.amount, exit_amount);
        assert_eq!(exit.zone_block, 1);
        assert_eq!(exit.exit_index, 0);

        // Verify exit is in the database
        let pending_exits = db.get_pending_exits().unwrap();
        assert_eq!(pending_exits.len(), 1);
    }
}
