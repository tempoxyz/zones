//! L2 Block Processor.
//!
//! Extracts deposits from L1 blocks and executes L2 blocks.

use crate::{L2Database, PzNodeTypes, execution, types::PzNodeTypesDb};
use alloy_consensus::BlockHeader as _;
use reth_primitives_traits as _;
use reth_provider::{BlockNumReader, Chain, ProviderFactory};
use std::sync::Arc;
use tracing::{debug, info, instrument};

/// L2 Block Processor.
///
/// Handles extraction of deposits from L1 and execution of L2 blocks.
pub struct PzBlockProcessor<Db>
where
    Db: PzNodeTypesDb,
{
    /// L2 chain spec.
    #[allow(dead_code)] // Will be used for block execution
    chain_spec: Arc<reth_chainspec::ChainSpec>,

    /// L2 provider factory for database access.
    l2_provider: ProviderFactory<PzNodeTypes<Db>>,

    /// In-memory L2 database for EVM execution.
    l2_db: std::sync::Mutex<L2Database>,
}

impl<Db: PzNodeTypesDb> std::fmt::Debug for PzBlockProcessor<Db> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PzBlockProcessor").finish()
    }
}

impl<Db: PzNodeTypesDb> PzBlockProcessor<Db> {
    /// Create a new block processor.
    pub fn new(
        chain_spec: Arc<reth_chainspec::ChainSpec>,
        l2_provider: ProviderFactory<PzNodeTypes<Db>>,
    ) -> Self {
        Self {
            chain_spec,
            l2_provider,
            l2_db: std::sync::Mutex::new(L2Database::default()),
        }
    }

    /// Process a committed L1 chain - extract deposits and execute L2 blocks.
    #[instrument(skip_all, fields(l1_blocks = chain.len()))]
    pub async fn on_l1_commit<P>(
        &self,
        chain: &Arc<Chain<P>>,
    ) -> eyre::Result<()>
    where
        P: reth_primitives_traits::NodePrimitives,
    {
        let _last_l2_height = self.l2_provider.last_block_number()?;

        for (block, receipts) in chain.blocks_and_receipts() {
            let l1_block_number = block.header().number();
            let l1_timestamp = block.header().timestamp();

            debug!(
                l1_block = l1_block_number,
                l1_timestamp, "Processing L1 block for deposits"
            );

            // Extract deposits from L1 receipts
            let deposits = self.extract_deposits(block, receipts);

            if deposits.is_empty() {
                continue;
            }

            info!(
                l1_block = l1_block_number,
                deposit_count = deposits.len(),
                "Found deposits in L1 block"
            );

            // Process deposits by crediting L2 balances
            {
                let mut db = self.l2_db.lock().expect("poisoned lock");
                for deposit in &deposits {
                    debug!(?deposit, "Processing deposit");
                    execution::process_deposit(&mut db, deposit.to, deposit.amount)?;
                    info!(
                        to = %deposit.to,
                        amount = %deposit.amount,
                        "Credited deposit to L2 account"
                    );
                }
            }

            // TODO: Execute L2 block with user transactions from the mempool
            // This would call execution::execute_block() with transactions
        }

        Ok(())
    }

    /// Extract deposit events from an L1 block's receipts.
    fn extract_deposits<B, R>(&self, _block: &B, _receipts: &[R]) -> Vec<DepositEvent> {
        // TODO: Implement deposit extraction from logs
        // Look for DepositEnqueued events from the ZonePortal contract
        Vec::new()
    }
}

/// A deposit event extracted from L1.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields will be used when deposit extraction is implemented
pub(crate) struct DepositEvent {
    /// L1 block number where deposit occurred.
    pub l1_block_number: u64,
    /// L1 block hash.
    pub l1_block_hash: alloy_primitives::B256,
    /// Sender on L1.
    pub sender: alloy_primitives::Address,
    /// Recipient on L2.
    pub to: alloy_primitives::Address,
    /// Amount deposited.
    pub amount: alloy_primitives::U256,
}
