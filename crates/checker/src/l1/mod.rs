//! Collection of all temporary evidence for one anchored Tempo/L1 block.

mod events;

use std::fmt;

use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockId, BlockNumHash, NumHash};
use alloy_network::{BlockResponse as _, ReceiptResponse as _, primitives::HeaderResponse as _};
use alloy_primitives::{Address, B256};
use alloy_provider::{DynProvider, Provider};
use eyre::WrapErr as _;
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionReceipt};
use tempo_contracts::precompiles::ITIP20;

pub(crate) use events::L1PortalEvent;
use events::{EventCollector, L1Events};

/// Recognized Portal events for one exact anchored L1 block.
#[derive(Debug)]
pub(crate) struct L1BlockEvidence {
    block: BlockNumHash,
    events: L1Events,
}

impl L1BlockEvidence {
    /// Return token specs from `TokenEnabled` events in canonical L1 event order.
    pub(crate) fn token_enabled_specs(&self) -> Vec<crate::model::TokenSpec> {
        self.events.token_enabled_specs()
    }

    /// Return authenticated Portal events in receipt order.
    pub(crate) fn portal_events(&self) -> impl Iterator<Item = &L1PortalEvent> {
        self.events.events.iter().map(|evidence| &evidence.event)
    }
}

impl fmt::Display for L1BlockEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut deposit_made = 0u64;
        let mut token_enabled = 0u64;
        let mut batch_submitted = 0u64;
        let mut withdrawal_processed = 0u64;
        let mut withdrawal_bounce_back = 0u64;
        let mut deposit_bounce_back = 0u64;
        let mut deposit_bounce_back_pending = 0u64;
        let mut refund_claimed = 0u64;

        for evidence in &self.events.events {
            match &evidence.event {
                L1PortalEvent::DepositMade { .. } => deposit_made += 1,
                L1PortalEvent::TokenEnabled { .. } => token_enabled += 1,
                L1PortalEvent::BatchSubmitted { .. } => batch_submitted += 1,
                L1PortalEvent::WithdrawalProcessed { .. } => withdrawal_processed += 1,
                L1PortalEvent::WithdrawalBounceBack { .. } => withdrawal_bounce_back += 1,
                L1PortalEvent::DepositBounceBack { .. } => deposit_bounce_back += 1,
                L1PortalEvent::DepositBounceBackPending { .. } => deposit_bounce_back_pending += 1,
                L1PortalEvent::RefundClaimed { .. } => refund_claimed += 1,
            }
        }
        write!(
            f,
            "L1 Portal facts extracted l1_block_number={} l1_block_hash={} portal={} \
             deposit_made={} token_enabled={} batch_submitted={} withdrawal_processed={} \
             withdrawal_bounce_back={} deposit_bounce_back={} deposit_bounce_back_pending={} \
             refund_claimed={}",
            self.block.number,
            self.block.hash,
            self.events.portal,
            deposit_made,
            token_enabled,
            batch_submitted,
            withdrawal_processed,
            withdrawal_bounce_back,
            deposit_bounce_back,
            deposit_bounce_back_pending,
            refund_claimed,
        )
    }
}

/// Fetch every canonical Tempo block after `parent` through `tip`.
pub(crate) async fn collect_l1_history(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    parent: BlockNumHash,
    tip: BlockNumHash,
) -> eyre::Result<Vec<L1BlockEvidence>> {
    eyre::ensure!(tip.number >= parent.number, "Tempo anchor moved backwards");
    let mut previous = parent;
    let mut history = Vec::with_capacity((tip.number - parent.number) as usize);
    for number in parent.number + 1..=tip.number {
        let block = provider
            .get_block_by_number(number.into())
            .hashes()
            .await?
            .ok_or_else(|| eyre::eyre!("Tempo block {number} is unavailable"))?;
        eyre::ensure!(
            block.header().parent_hash() == previous.hash,
            "Tempo history is not contiguous at block {number}"
        );
        let coordinate = BlockNumHash::new(number, block.header().hash());
        history.push(collect_l1_block_at(provider, portal, coordinate).await?);
        previous = coordinate;
    }
    eyre::ensure!(
        previous == tip,
        "Tempo history does not end at the Zone anchor"
    );
    Ok(history)
}

/// Read Portal custody for one token at an exact canonical Tempo block.
pub(crate) async fn portal_balance(
    provider: &DynProvider<TempoNetwork>,
    token: Address,
    portal: Address,
    block: B256,
) -> eyre::Result<alloy_primitives::U256> {
    Ok(ITIP20::new(token, provider)
        .balanceOf(portal)
        .block(BlockId::hash_canonical(block))
        .call()
        .await?)
}

async fn collect_l1_block_at(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    block: BlockNumHash,
) -> eyre::Result<L1BlockEvidence> {
    let hash = block.hash;
    let number = block.number;
    let block = provider
        .get_block_by_hash(hash)
        .hashes()
        .await
        .wrap_err_with(|| format!("failed to fetch L1 block {number} ({hash})"))?
        .ok_or_else(|| eyre::eyre!("L1 block {number} ({hash}) not found"))?;

    authenticate_l1_block(hash, number, block.header().hash(), block.header().number())?;

    let transaction_hashes = block.transactions().as_hashes().ok_or_else(|| {
        eyre::eyre!("L1 block {number} ({hash}) did not contain transaction hashes")
    })?;

    let receipts = provider
        .get_block_receipts(BlockId::hash(hash))
        .await
        .wrap_err_with(|| format!("failed to fetch L1 receipts for block {number} ({hash})"))?
        .ok_or_else(|| eyre::eyre!("no receipts for L1 block {number} ({hash})"))?;
    validate_l1_receipts(NumHash::new(number, hash), transaction_hashes, &receipts)?;

    let mut event_collector = EventCollector::new(portal);
    for (index, (receipt, transaction_hash)) in receipts.iter().zip(transaction_hashes).enumerate()
    {
        event_collector.extract_receipt(index, *transaction_hash, receipt, number)?;
    }
    let events = event_collector.finish();
    Ok(L1BlockEvidence {
        block: BlockNumHash::new(number, hash),
        events,
    })
}

/// Verify that a fetched L1 block matches the exact hash and number from the
/// `TempoAdvanced` anchor.  This prevents using a latest/head lookup or a
/// different fork after the anchor is obtained.
fn authenticate_l1_block(
    anchor_hash: B256,
    anchor_number: u64,
    rpc_hash: B256,
    block_number: u64,
) -> eyre::Result<()> {
    eyre::ensure!(
        rpc_hash == anchor_hash,
        "L1 block hash mismatch: anchor {anchor_hash}, RPC returned {rpc_hash}"
    );
    eyre::ensure!(
        block_number == anchor_number,
        "L1 block number mismatch: anchor {anchor_number}, fetched {block_number}"
    );
    Ok(())
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
    use alloy_consensus::{Header, ReceiptWithBloom, Sealable as _};
    use alloy_primitives::{B256, Bloom};
    use alloy_rpc_types_eth::{Header as RpcHeader, TransactionReceipt};
    use tempo_alloy::rpc::{TempoHeaderResponse, TempoTransactionReceipt};
    use tempo_primitives::{TempoHeader, TempoReceipt, TempoTxType};

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
    fn authenticate_l1_block_validates_hash_and_number() {
        let header = TempoHeader {
            inner: Header {
                number: BLOCK,
                ..Default::default()
            },
            ..Default::default()
        };
        let hash = header.hash_slow();
        let response = TempoHeaderResponse {
            inner: RpcHeader {
                hash,
                inner: header,
                total_difficulty: None,
                size: None,
            },
            timestamp_millis: 0,
        };

        assert!(authenticate_l1_block(hash, BLOCK, response.inner.hash, BLOCK).is_ok());
        assert!(authenticate_l1_block(hash, BLOCK, HASH, BLOCK).is_err());
        assert!(authenticate_l1_block(hash, BLOCK, hash, BLOCK + 1).is_err());
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
