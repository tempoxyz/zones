//! Collection of protocol evidence from canonical L2 blocks.

mod events;
mod state;

use std::collections::{BTreeMap, BTreeSet};

use alloy_consensus::{TxReceipt, transaction::TxHashRef};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, Log};

use events::EventCollector;

pub(crate) use events::{
    DepositResult, L1Anchor, L2BridgeAction, TokenTransfer, WithdrawalBounceBackStatus,
    WithdrawalOrigin,
};
pub(crate) use state::{read_accounting_state, read_zone_genesis};

/// Authenticated anchor, transfers, and bridge actions from one L2 block.
#[derive(Debug)]
pub(crate) struct L2BlockEvidence {
    anchor: L1Anchor,
    transfers: Vec<TokenTransfer>,
    actions: Vec<L2BridgeAction>,
}

impl L2BlockEvidence {
    /// Return the exact Tempo block imported by this L2 block.
    pub(crate) const fn l1_anchor(&self) -> &L1Anchor {
        &self.anchor
    }

    /// Return accounts named by canonical TIP-20 transfers, grouped by token.
    pub(crate) fn accounting_candidates(&self) -> BTreeMap<Address, BTreeSet<Address>> {
        let mut candidates = BTreeMap::<Address, BTreeSet<Address>>::new();
        for transfer in &self.transfers {
            let accounts = candidates.entry(transfer.token).or_default();
            if !transfer.from.is_zero() {
                accounts.insert(transfer.from);
            }
            if !transfer.to.is_zero() {
                accounts.insert(transfer.to);
            }
        }
        candidates
    }

    /// Return canonical TIP-20 transfers in block-log order.
    pub(crate) fn token_transfers(&self) -> impl Iterator<Item = TokenTransfer> + '_ {
        self.transfers.iter().copied()
    }

    /// Return authenticated bridge actions in block-log order.
    pub(crate) fn bridge_actions(&self) -> impl Iterator<Item = &L2BridgeAction> {
        self.actions.iter()
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
    for (transaction, receipt) in transactions.iter().zip(receipts) {
        collector.extract_receipt(transaction, receipt, block.number)?;
    }

    collector.finish(block.number)
}
