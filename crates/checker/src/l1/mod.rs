//! Collection of all temporary evidence for one anchored Tempo/L1 block.

mod events;

use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockId, BlockNumHash, NumHash};
use alloy_network::{BlockResponse as _, ReceiptResponse as _, primitives::HeaderResponse as _};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_transport::{RpcError, TransportError, TransportErrorKind};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionReceipt};
use tempo_contracts::precompiles::ITIP20;

use crate::AttemptError;

pub(crate) use events::L1PortalEvent;
use events::{EventCollector, L1Events};

/// Bound on concurrent Portal balance reads for one token set.
const BALANCE_CONCURRENCY: usize = 8;

/// Failure acquiring or interpreting exact Tempo state.
#[derive(Debug)]
pub(crate) enum L1ReadError {
    /// Required RPC data is not currently available.
    Unavailable(eyre::Report),
    /// Authenticated protocol evidence cannot be verified.
    Finding(eyre::Report),
    /// Deterministic provider or checker failure prevents verification.
    Disable(eyre::Report),
}

impl From<AttemptError> for L1ReadError {
    fn from(error: AttemptError) -> Self {
        match error {
            AttemptError::Retry(error) => Self::Unavailable(error),
            AttemptError::Disable(error) => Self::Disable(error),
        }
    }
}

impl From<L1ReadError> for AttemptError {
    fn from(error: L1ReadError) -> Self {
        match error {
            L1ReadError::Unavailable(error) => Self::Retry(error),
            L1ReadError::Finding(error) | L1ReadError::Disable(error) => Self::Disable(error),
        }
    }
}

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
) -> Result<L1BlockEvidence, L1ReadError> {
    collect_l1_block_inner(provider, portal, parent, None).await
}

/// Fetch the exact anchored Tempo block that extends `parent`.
pub(crate) async fn collect_l1_block_at(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    parent: BlockNumHash,
    expected: BlockNumHash,
) -> Result<L1BlockEvidence, L1ReadError> {
    collect_l1_block_inner(provider, portal, parent, Some(expected)).await
}

async fn collect_l1_block_inner(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    parent: BlockNumHash,
    expected: Option<BlockNumHash>,
) -> Result<L1BlockEvidence, L1ReadError> {
    let number = parent.number.checked_add(1).ok_or_else(|| {
        disable(eyre::eyre!(
            "Tempo block number overflow after {}",
            parent.number
        ))
    })?;
    let block = provider
        .get_block_by_number(number.into())
        .hashes()
        .await
        .map_err(classify_rpc_error)?
        .ok_or_else(|| unavailable(eyre::eyre!("Tempo block {number} is unavailable")))?;
    if block.header().number() != number {
        return Err(disable(eyre::eyre!(
            "Tempo RPC returned block {} for requested block {number}",
            block.header().number()
        )));
    }
    if block.header().parent_hash() != parent.hash {
        return Err(finding(eyre::eyre!(
            "Tempo history is not contiguous at block {number}"
        )));
    }
    let coordinate = BlockNumHash::new(number, block.header().hash());
    if expected.is_some_and(|expected| coordinate != expected) {
        return Err(finding(eyre::eyre!(
            "Tempo history does not end at the Zone anchor"
        )));
    }
    let transaction_hashes = block.transactions().as_hashes().ok_or_else(|| {
        disable(eyre::eyre!(
            "Tempo block {number} ({}) did not contain transaction hashes",
            coordinate.hash
        ))
    })?;
    collect_l1_block_evidence(provider, portal, coordinate, transaction_hashes).await
}

/// Read Portal custody for one token at an exact canonical Tempo block.
pub(crate) async fn portal_balance(
    provider: &DynProvider<TempoNetwork>,
    token: Address,
    portal: Address,
    block: B256,
) -> Result<U256, L1ReadError> {
    ITIP20::new(token, provider)
        .balanceOf(portal)
        .block(BlockId::hash_canonical(block))
        .call()
        .await
        .map_err(classify_contract_error)
}

/// Read Portal custody for every token, concurrently, at one exact canonical Tempo block.
pub(crate) async fn portal_balances(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    tokens: impl IntoIterator<Item = Address>,
    block: B256,
) -> Result<Vec<(Address, U256)>, L1ReadError> {
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
) -> Result<L1BlockEvidence, L1ReadError> {
    let hash = block.hash;
    let number = block.number;
    let receipts = provider
        .get_block_receipts(BlockId::hash(hash))
        .await
        .map_err(classify_rpc_error)?
        .ok_or_else(|| unavailable(eyre::eyre!("no receipts for L1 block {number} ({hash})")))?;
    validate_l1_receipts(NumHash::new(number, hash), transaction_hashes, &receipts)
        .map_err(disable)?;

    let mut event_collector = EventCollector::new(portal);
    for receipt in &receipts {
        event_collector
            .extract_receipt(receipt, number)
            .map_err(finding)?;
    }
    let events = event_collector.finish();
    Ok(L1BlockEvidence { block, events })
}

fn classify_contract_error(error: alloy_contract::Error) -> L1ReadError {
    if error.as_revert_data().is_some() {
        return finding(error);
    }
    match error {
        alloy_contract::Error::TransportError(error) => classify_rpc_error(error).into(),
        error @ (alloy_contract::Error::ContractNotDeployed
        | alloy_contract::Error::ZeroData(..)
        | alloy_contract::Error::AbiError(_)) => finding(error),
        error => disable(error),
    }
}

/// Classify one provider RPC failure without relying on its display text.
pub(crate) fn classify_rpc_error(error: TransportError) -> AttemptError {
    let retryable = match &error {
        RpcError::ErrorResp(error) => error.is_retry_err(),
        RpcError::UnsupportedFeature(_)
        | RpcError::LocalUsageError(_)
        | RpcError::SerError(_)
        | RpcError::DeserError { .. }
        | RpcError::Transport(TransportErrorKind::NonRetryable(_)) => false,
        _ => true,
    };
    if retryable {
        AttemptError::retry(error)
    } else {
        AttemptError::disable(error)
    }
}

fn unavailable(error: impl Into<eyre::Report>) -> L1ReadError {
    L1ReadError::Unavailable(error.into())
}

fn finding(error: impl Into<eyre::Report>) -> L1ReadError {
    L1ReadError::Finding(error.into())
}

fn disable(error: impl Into<eyre::Report>) -> L1ReadError {
    L1ReadError::Disable(error.into())
}

/// Validate transaction and receipt correspondence from the trusted L1 RPC.
pub(crate) fn validate_l1_receipts(
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

    #[test]
    fn classifies_retryable_and_terminal_transport_failures() {
        let retryable = alloy_transport::TransportErrorKind::backend_gone();
        assert!(matches!(
            classify_rpc_error(retryable),
            AttemptError::Retry(_)
        ));

        let terminal = alloy_transport::TransportErrorKind::non_retryable_str("invalid request");
        assert!(matches!(
            classify_rpc_error(terminal),
            AttemptError::Disable(_)
        ));
    }
}
