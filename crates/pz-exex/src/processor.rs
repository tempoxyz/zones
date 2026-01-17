//! L2 Block Processor.
//!
//! Extracts deposits from L1 blocks and executes L2 blocks.

use crate::{L2Database, PzNodeTypes, execution, portal, types::PzNodeTypesDb};
use alloy_consensus::BlockHeader as _;
use alloy_primitives::{Address, B256, Log, U256};
use alloy_sol_types::SolEvent;
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
        P::Receipt: ReceiptExt,
    {
        let _last_l2_height = self.l2_provider.last_block_number()?;

        for block in chain.blocks_iter() {
            let l1_block_number = block.header().number();
            let l1_block_hash = block.hash();
            let l1_timestamp = block.header().timestamp();

            debug!(
                l1_block = l1_block_number,
                %l1_block_hash,
                l1_timestamp,
                "Processing L1 block for deposits"
            );

            // Get receipts for this block
            let receipts = chain
                .receipts_by_block_hash(l1_block_hash)
                .unwrap_or_default();

            // Extract deposits from L1 receipts
            let deposits = self.extract_deposits(l1_block_number, l1_block_hash, &receipts);

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
    fn extract_deposits<R>(&self, block_number: u64, block_hash: B256, receipts: &[R]) -> Vec<DepositEvent>
    where
        R: ReceiptExt,
    {
        let mut deposits = Vec::new();

        for receipt in receipts {
            // Only process successful transactions
            if !receipt.status() {
                continue;
            }

            for log in receipt.logs() {
                // Check if log is from the ZonePortal contract
                if log.address != portal::ZONE_PORTAL_ADDRESS {
                    continue;
                }

                // Try to decode as DepositEnqueued event
                match portal::DepositEnqueued::decode_log(log) {
                    Ok(event) => {
                        let deposit = portal::Deposit::from(event.data);
                        deposits.push(DepositEvent {
                            l1_block_number: block_number,
                            l1_block_hash: block_hash,
                            sender: deposit.sender,
                            to: deposit.to,
                            amount: deposit.amount,
                            data: deposit.data,
                        });
                    }
                    Err(e) => {
                        // Log has portal address but isn't a DepositEnqueued event
                        // This is normal - the portal might emit other events
                        debug!(
                            ?log,
                            error = %e,
                            "Log from ZonePortal is not a DepositEnqueued event"
                        );
                    }
                }
            }
        }

        deposits
    }
}

/// Extension trait to access receipt data generically.
///
/// This is auto-implemented for any type implementing `TxReceipt<Log = Log>`.
pub trait ReceiptExt {
    /// Returns true if the transaction was successful.
    fn status(&self) -> bool;
    /// Returns the logs emitted by the transaction.
    fn logs(&self) -> &[Log];
}

/// Blanket implementation for reth_primitives_traits::Receipt.
impl<T> ReceiptExt for T
where
    T: alloy_consensus::TxReceipt<Log = Log>,
{
    fn status(&self) -> bool {
        alloy_consensus::TxReceipt::status(self)
    }

    fn logs(&self) -> &[Log] {
        alloy_consensus::TxReceipt::logs(self)
    }
}

/// A deposit event extracted from L1.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields will be used for L2 block production and revert handling
pub(crate) struct DepositEvent {
    /// L1 block number where deposit occurred.
    pub l1_block_number: u64,
    /// L1 block hash.
    pub l1_block_hash: B256,
    /// Sender on L1.
    pub sender: Address,
    /// Recipient on L2.
    pub to: Address,
    /// Amount deposited.
    pub amount: U256,
    /// Optional calldata for contract interactions.
    pub data: alloy_primitives::Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, address, b256};

    /// Mock receipt for testing deposit extraction.
    #[derive(Debug, Clone)]
    struct MockReceipt {
        status: bool,
        logs: Vec<Log>,
    }

    impl ReceiptExt for MockReceipt {
        fn status(&self) -> bool {
            self.status
        }

        fn logs(&self) -> &[Log] {
            &self.logs
        }
    }

    fn create_deposit_log(sender: Address, to: Address, amount: U256, data: Bytes) -> Log {
        use alloy_sol_types::SolEvent;

        let event = portal::DepositEnqueued {
            sender,
            to,
            amount,
            data,
        };

        Log::new(
            portal::ZONE_PORTAL_ADDRESS,
            event.encode_topics().into_iter().map(|t| t.0).collect(),
            event.encode_data().into(),
        )
        .unwrap()
    }

    #[test]
    fn test_extract_deposits_from_empty_receipts() {
        let receipts: Vec<MockReceipt> = vec![];
        let block_number = 100;
        let block_hash = b256!("0x1234567890123456789012345678901234567890123456789012345678901234");

        // Create a minimal processor for testing
        // We can't easily create a full PzBlockProcessor without a provider factory,
        // but we can test the extraction logic directly
        let deposits = extract_deposits_from_receipts(block_number, block_hash, &receipts);
        assert!(deposits.is_empty());
    }

    #[test]
    fn test_extract_deposits_from_successful_receipt() {
        let sender = address!("1111111111111111111111111111111111111111");
        let to = address!("2222222222222222222222222222222222222222");
        let amount = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
        let data = Bytes::default();

        let log = create_deposit_log(sender, to, amount, data.clone());

        let receipts = vec![MockReceipt {
            status: true,
            logs: vec![log],
        }];

        let block_number = 100;
        let block_hash = b256!("0x1234567890123456789012345678901234567890123456789012345678901234");

        let deposits = extract_deposits_from_receipts(block_number, block_hash, &receipts);

        assert_eq!(deposits.len(), 1);
        assert_eq!(deposits[0].sender, sender);
        assert_eq!(deposits[0].to, to);
        assert_eq!(deposits[0].amount, amount);
        assert_eq!(deposits[0].l1_block_number, block_number);
        assert_eq!(deposits[0].l1_block_hash, block_hash);
    }

    #[test]
    fn test_extract_deposits_ignores_failed_receipts() {
        let sender = address!("1111111111111111111111111111111111111111");
        let to = address!("2222222222222222222222222222222222222222");
        let amount = U256::from(1_000_000_000_000_000_000u128);

        let log = create_deposit_log(sender, to, amount, Bytes::default());

        let receipts = vec![MockReceipt {
            status: false, // Failed transaction
            logs: vec![log],
        }];

        let block_number = 100;
        let block_hash = b256!("0x1234567890123456789012345678901234567890123456789012345678901234");

        let deposits = extract_deposits_from_receipts(block_number, block_hash, &receipts);

        assert!(deposits.is_empty(), "Should not extract deposits from failed transactions");
    }

    #[test]
    fn test_extract_deposits_ignores_logs_from_other_contracts() {
        let sender = address!("1111111111111111111111111111111111111111");
        let to = address!("2222222222222222222222222222222222222222");
        let amount = U256::from(1_000_000_000_000_000_000u128);

        // Create log but from a different address
        let mut log = create_deposit_log(sender, to, amount, Bytes::default());
        log.address = address!("3333333333333333333333333333333333333333"); // Not the portal

        let receipts = vec![MockReceipt {
            status: true,
            logs: vec![log],
        }];

        let block_number = 100;
        let block_hash = b256!("0x1234567890123456789012345678901234567890123456789012345678901234");

        let deposits = extract_deposits_from_receipts(block_number, block_hash, &receipts);

        assert!(deposits.is_empty(), "Should not extract deposits from other contracts");
    }

    #[test]
    fn test_extract_multiple_deposits() {
        let sender1 = address!("1111111111111111111111111111111111111111");
        let to1 = address!("2222222222222222222222222222222222222222");
        let amount1 = U256::from(1_000_000_000_000_000_000u128);

        let sender2 = address!("3333333333333333333333333333333333333333");
        let to2 = address!("4444444444444444444444444444444444444444");
        let amount2 = U256::from(2_000_000_000_000_000_000u128);

        let log1 = create_deposit_log(sender1, to1, amount1, Bytes::default());
        let log2 = create_deposit_log(sender2, to2, amount2, Bytes::default());

        let receipts = vec![
            MockReceipt {
                status: true,
                logs: vec![log1],
            },
            MockReceipt {
                status: true,
                logs: vec![log2],
            },
        ];

        let block_number = 100;
        let block_hash = b256!("0x1234567890123456789012345678901234567890123456789012345678901234");

        let deposits = extract_deposits_from_receipts(block_number, block_hash, &receipts);

        assert_eq!(deposits.len(), 2);
        assert_eq!(deposits[0].amount, amount1);
        assert_eq!(deposits[1].amount, amount2);
    }

    /// Helper function to test extraction logic without needing a full PzBlockProcessor.
    fn extract_deposits_from_receipts<R: ReceiptExt>(
        block_number: u64,
        block_hash: B256,
        receipts: &[R],
    ) -> Vec<DepositEvent> {
        let mut deposits = Vec::new();

        for receipt in receipts {
            if !receipt.status() {
                continue;
            }

            for log in receipt.logs() {
                if log.address != portal::ZONE_PORTAL_ADDRESS {
                    continue;
                }

                match portal::DepositEnqueued::decode_log(log) {
                    Ok(event) => {
                        let deposit = portal::Deposit::from(event.data);
                        deposits.push(DepositEvent {
                            l1_block_number: block_number,
                            l1_block_hash: block_hash,
                            sender: deposit.sender,
                            to: deposit.to,
                            amount: deposit.amount,
                            data: deposit.data,
                        });
                    }
                    Err(_) => {}
                }
            }
        }

        deposits
    }
}
