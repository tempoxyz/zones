//! Collection of all temporary evidence for one anchored Tempo/L1 block.

mod events;

use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockId, BlockNumHash, NumHash};
use alloy_network::{BlockResponse as _, ReceiptResponse as _, primitives::HeaderResponse as _};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider};
use eyre::WrapErr as _;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionReceipt};
use tempo_contracts::precompiles::ITIP20;

pub(crate) use events::L1PortalEvent;
use events::{EventCollector, L1Events};

/// Bound on concurrent Portal balance reads for one token set.
const BALANCE_CONCURRENCY: usize = 8;

/// Recognized Portal events for one exact anchored L1 block.
#[derive(Debug)]
pub(crate) struct L1BlockEvidence {
    block: BlockNumHash,
    events: L1Events,
}

impl L1BlockEvidence {
    pub(crate) const fn block(&self) -> BlockNumHash {
        self.block
    }

    /// Return authenticated Portal events in receipt order.
    pub(crate) fn portal_events(&self) -> impl Iterator<Item = &L1PortalEvent> {
        self.events.events.iter()
    }
}

/// Fetch one canonical Tempo block that extends `parent`.
pub(crate) async fn collect_l1_block(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    parent: BlockNumHash,
) -> eyre::Result<L1BlockEvidence> {
    let number = parent
        .number
        .checked_add(1)
        .ok_or_else(|| eyre::eyre!("Tempo block number overflow after {}", parent.number))?;
    let block = provider
        .get_block_by_number(number.into())
        .hashes()
        .await
        .wrap_err_with(|| format!("failed to fetch Tempo block {number}"))?
        .ok_or_else(|| eyre::eyre!("Tempo block {number} is unavailable"))?;
    eyre::ensure!(
        block.header().number() == number,
        "Tempo RPC returned block {} for requested block {number}",
        block.header().number()
    );
    eyre::ensure!(
        block.header().parent_hash() == parent.hash,
        "Tempo history is not contiguous at block {number}"
    );
    let coordinate = BlockNumHash::new(number, block.header().hash());
    let transaction_hashes = block.transactions().as_hashes().ok_or_else(|| {
        eyre::eyre!(
            "Tempo block {number} ({}) did not contain transaction hashes",
            coordinate.hash
        )
    })?;
    collect_l1_block_evidence(provider, portal, coordinate, transaction_hashes).await
}

/// Read Portal custody for one token at an exact canonical Tempo block.
pub(crate) async fn portal_balance(
    provider: &DynProvider<TempoNetwork>,
    token: Address,
    portal: Address,
    block: B256,
) -> eyre::Result<U256> {
    Ok(ITIP20::new(token, provider)
        .balanceOf(portal)
        .block(BlockId::hash_canonical(block))
        .call()
        .await?)
}

/// Read Portal custody for every token, concurrently, at one exact canonical Tempo block.
pub(crate) async fn portal_balances(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    tokens: impl IntoIterator<Item = Address>,
    block: B256,
) -> eyre::Result<Vec<(Address, U256)>> {
    stream::iter(tokens.into_iter().map(|token| async move {
        portal_balance(provider, token, portal, block)
            .await
            .map(|balance| (token, balance))
    }))
    .buffer_unordered(BALANCE_CONCURRENCY)
    .try_collect()
    .await
}

/// Fetch receipts and collect Portal events for one authenticated L1 block.
async fn collect_l1_block_evidence(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    block: BlockNumHash,
    transaction_hashes: &[B256],
) -> eyre::Result<L1BlockEvidence> {
    let hash = block.hash;
    let number = block.number;
    let receipts = provider
        .get_block_receipts(BlockId::hash(hash))
        .await
        .wrap_err_with(|| format!("failed to fetch L1 receipts for block {number} ({hash})"))?
        .ok_or_else(|| eyre::eyre!("no receipts for L1 block {number} ({hash})"))?;
    validate_l1_receipts(NumHash::new(number, hash), transaction_hashes, &receipts)?;

    let mut event_collector = EventCollector::new(portal);
    for receipt in &receipts {
        event_collector.extract_receipt(receipt, number)?;
    }
    let events = event_collector.finish();
    Ok(L1BlockEvidence { block, events })
}

/// Validate transaction and receipt correspondence from the trusted L1 RPC.
fn validate_l1_receipts(
    block: NumHash,
    transaction_hashes: &[B256],
    receipts: &[TempoTransactionReceipt],
) -> eyre::Result<()> {
    let block_number = block.number;
    let block_hash = block.hash;
    eyre::ensure!(
        receipts.len() == transaction_hashes.len(),
        "L1 block {block_number} ({block_hash}) has {} transactions but {} receipts",
        transaction_hashes.len(),
        receipts.len()
    );
    for (index, (transaction_hash, receipt)) in transaction_hashes.iter().zip(receipts).enumerate()
    {
        eyre::ensure!(
            receipt.block_hash() == Some(block_hash),
            "receipt {index} has wrong block hash in L1 block {block_number} ({block_hash})"
        );
        eyre::ensure!(
            receipt.block_number() == Some(block_number),
            "receipt {index} has wrong block number in L1 block {block_number} ({block_hash})"
        );
        eyre::ensure!(
            receipt.transaction_index() == Some(index as u64),
            "receipt {index} has wrong transaction index in L1 block {block_number} ({block_hash})"
        );
        eyre::ensure!(
            receipt.transaction_hash() == *transaction_hash,
            "receipt {index} has wrong transaction hash in L1 block {block_number} ({block_hash})"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::ReceiptWithBloom;
    use alloy_primitives::{B256, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use tempo_alloy::rpc::TempoTransactionReceipt;
    use tempo_primitives::{TempoReceipt, TempoTxType};

    const BLOCK: u64 = 100;
    const HASH: B256 = B256::repeat_byte(0x10);

    fn receipt_with_logs(
        success: bool,
        logs: Vec<alloy_rpc_types_eth::Log>,
    ) -> TempoTransactionReceipt {
        let inner_receipt = TempoReceipt {
            tx_type: TempoTxType::Legacy,
            success,
            cumulative_gas_used: 0,
            logs,
        };
        TempoTransactionReceipt {
            inner: TransactionReceipt {
                inner: ReceiptWithBloom::new(inner_receipt, Bloom::ZERO),
                transaction_hash: B256::ZERO,
                transaction_index: Some(0),
                block_hash: Some(HASH),
                block_number: Some(BLOCK),
                gas_used: 0,
                effective_gas_price: 0,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: Some(Address::ZERO),
                contract_address: None,
            },
            fee_token: None,
            fee_payer: Address::ZERO,
        }
    }

    #[test]
    fn validate_receipts_rejects_count_mismatch() {
        let receipts = [receipt_with_logs(true, vec![])];
        assert!(
            validate_l1_receipts(
                NumHash::new(BLOCK, HASH),
                &[B256::ZERO, B256::repeat_byte(1)],
                &receipts,
            )
            .is_err()
        );
    }

    #[test]
    fn validate_receipts_rejects_wrong_transaction_metadata() {
        let receipts = [receipt_with_logs(true, vec![])];
        assert!(
            validate_l1_receipts(
                NumHash::new(BLOCK, HASH),
                &[B256::repeat_byte(1)],
                &receipts,
            )
            .is_err()
        );
    }
}
