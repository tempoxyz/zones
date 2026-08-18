//! Collection of protocol evidence from canonical L2 blocks.

mod events;
mod state;

use std::collections::{BTreeMap, BTreeSet};

use alloy_consensus::{TxReceipt, transaction::TxHashRef};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, Log};

use events::{EventCollector, L2Events};

pub(crate) use events::{L1Anchor, L2BridgeEvent};
pub(crate) use state::{TokenAccountingEvidence, read_accounting_state, read_zone_genesis};

/// Recognized protocol events from one L2 block.
#[derive(Debug)]
pub(crate) struct L2BlockEvidence {
    events: L2Events,
}

impl L2BlockEvidence {
    /// Return the exact Tempo block imported by this L2 block.
    pub(crate) fn l1_anchor(&self) -> Option<&L1Anchor> {
        self.events.l1_anchor()
    }

    /// Return accounts named by canonical TIP-20 transfers, grouped by token.
    pub(crate) fn accounting_candidates(&self) -> BTreeMap<Address, BTreeSet<Address>> {
        let mut candidates = BTreeMap::<Address, BTreeSet<Address>>::new();
        for (token, from, to, _) in self.events.token_transfers() {
            let accounts = candidates.entry(token).or_default();
            if !from.is_zero() {
                accounts.insert(from);
            }
            if !to.is_zero() {
                accounts.insert(to);
            }
        }
        candidates
    }

    /// Return authenticated bridge events in block-log order.
    pub(crate) fn bridge_events(&self) -> impl Iterator<Item = &L2BridgeEvent> {
        self.events.events.iter().map(|evidence| &evidence.event)
    }
}

/// Collect recognized events from one canonical L2 block.
pub(crate) fn collect_l2_block_evidence<T, R>(
    transactions: &[T],
    receipts: &[R],
    block: BlockNumHash,
) -> eyre::Result<L2BlockEvidence>
where
    T: TxHashRef,
    R: TxReceipt<Log = Log>,
{
    eyre::ensure!(
        transactions.len() == receipts.len(),
        "block {} has {} transactions but {} receipts",
        block.number,
        transactions.len(),
        receipts.len()
    );

    let mut collector = EventCollector::default();
    for (transaction_index, (transaction, receipt)) in transactions.iter().zip(receipts).enumerate()
    {
        collector.extract_receipt(transaction_index, transaction, receipt, block.number)?;
    }

    Ok(L2BlockEvidence {
        events: collector.finish(block.number)?,
    })
}
