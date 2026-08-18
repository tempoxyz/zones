//! Collection of all temporary evidence for one canonical L2 block.

mod calldata;
mod events;
mod state;

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
};

use alloy_consensus::TxReceipt;
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, Log};
use reth_storage_api::StateProviderFactory;

use calldata::L2AdvanceTempoEvidence;
use events::{EventCollector, L2Events};

pub(crate) use events::L1Anchor;
pub(crate) use events::L2BridgeEvent;
pub(crate) use state::{L2TokenStateEvidence, read_accounting_state, read_zone_genesis};

/// Calldata, recognized events, and exact post-state for one L2 block.
#[derive(Debug)]
pub(crate) struct L2BlockEvidence {
    block: BlockNumHash,
    advance_tempo_calls: Vec<L2AdvanceTempoEvidence>,
    events: L2Events,
    token_states: Vec<L2TokenStateEvidence>,
}

impl L2BlockEvidence {
    /// Return the exact Tempo/L1 block imported by this L2 block.
    pub(crate) fn l1_anchor(&self) -> &L1Anchor {
        self.events.l1_anchor()
    }

    /// Return token specs from `TokenEnabled` events in canonical L2 event order.
    pub(crate) fn token_enabled_specs(&self) -> Vec<crate::model::TokenSpec> {
        self.events.token_enabled_specs()
    }

    /// Return the total number of `advanceTempo` calls in this block.
    pub(crate) fn advance_tempo_call_count(&self) -> usize {
        self.advance_tempo_calls.len()
    }

    /// Return transaction positions and senders for successful `advanceTempo` calls.
    pub(crate) fn successful_advance_tempo_provenance(&self) -> Vec<(u32, Address)> {
        self.advance_tempo_calls
            .iter()
            .filter(|call| call.success)
            .map(|call| (call.transaction_index, call.sender))
            .collect()
    }

    /// Return `enabledTokens` specs from all successful `advanceTempo` calls,
    /// in canonical call order.
    pub(crate) fn advance_tempo_enabled_token_specs(&self) -> Vec<crate::model::TokenSpec> {
        self.advance_tempo_calls
            .iter()
            .filter(|call| call.success)
            .flat_map(|call| call.enabled_tokens.iter())
            .map(|t| crate::model::TokenSpec {
                token: t.token,
                name: t.name.clone(),
                symbol: t.symbol.clone(),
                currency: t.currency.clone(),
            })
            .collect()
    }

    /// Return L2 token state observations.
    pub(crate) fn token_states(&self) -> &[L2TokenStateEvidence] {
        &self.token_states
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

impl fmt::Display for L2BlockEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut deposits_processed = 0u64;
        let mut deposits_failed = 0u64;
        let mut bounce_backs_processed = 0u64;
        let mut bounce_backs_pending = 0u64;
        let mut withdrawal_requests = 0u64;
        let mut enabled_tokens = 0u64;
        let mut refund_claims = 0u64;
        let mut transfers = 0u64;
        let mut batch_finalized = false;

        for event in &self.events.events {
            match &event.event {
                L2BridgeEvent::TempoAdvanced(_) => {}
                L2BridgeEvent::DepositOutcome {
                    processed: true, ..
                } => deposits_processed += 1,
                L2BridgeEvent::DepositOutcome {
                    processed: false, ..
                } => deposits_failed += 1,
                L2BridgeEvent::WithdrawalBounceBack {
                    processed: true, ..
                } => bounce_backs_processed += 1,
                L2BridgeEvent::WithdrawalBounceBack {
                    processed: false, ..
                } => bounce_backs_pending += 1,
                L2BridgeEvent::WithdrawalRequested { .. } => withdrawal_requests += 1,
                L2BridgeEvent::TokenEnabled { .. } => enabled_tokens += 1,
                L2BridgeEvent::RefundClaimed { .. } => refund_claims += 1,
                L2BridgeEvent::Transfer { .. } => transfers += 1,
                L2BridgeEvent::BatchFinalized { .. } => batch_finalized = true,
            }
        }

        let anchor = self.l1_anchor();
        let advance_tempo_succeeded = self
            .advance_tempo_calls
            .iter()
            .filter(|call| call.success)
            .count();
        let advance_tempo_enabled_tokens = self
            .advance_tempo_calls
            .iter()
            .filter(|call| call.success)
            .map(|call| call.enabled_tokens.len())
            .sum::<usize>();

        write!(
            f,
            "L2 bridge facts extracted l2_block_number={} l2_block_hash={} \
             l1_block_number={} l1_block_hash={} deposits_processed={} deposits_failed={} \
             bounce_backs_processed={} bounce_backs_pending={} withdrawal_requests={} \
             enabled_tokens={} refund_claims={} transfers={} batch_finalized={} advance_tempo_calls={} \
             advance_tempo_succeeded={} advance_tempo_enabled_tokens={} \
             token_enabled_events={} token_state_observations={}",
            self.block.number,
            self.block.hash,
            anchor.block_number(),
            anchor.block_hash(),
            deposits_processed,
            deposits_failed,
            bounce_backs_processed,
            bounce_backs_pending,
            withdrawal_requests,
            enabled_tokens,
            refund_claims,
            transfers,
            batch_finalized,
            self.advance_tempo_calls.len(),
            advance_tempo_succeeded,
            advance_tempo_enabled_tokens,
            enabled_tokens,
            self.token_states.len(),
        )
    }
}

/// Return tokens enabled by successful calls or events, deduplicated in first-seen order.
fn token_enablement_candidates(
    calls: &[L2AdvanceTempoEvidence],
    event_tokens: impl IntoIterator<Item = Address>,
) -> Vec<Address> {
    let mut seen = HashSet::new();
    calls
        .iter()
        .filter(|call| call.success)
        .flat_map(|call| call.enabled_tokens.iter().map(|token| token.token))
        .chain(event_tokens)
        .filter(|token| seen.insert(*token))
        .collect()
}

/// Collect the complete L2 evidence bundle from one canonical notification block.
pub(crate) fn collect_l2_block_evidence<P, T, R>(
    provider: &P,
    transactions: &[T],
    senders: &[Address],
    receipts: &[R],
    block: BlockNumHash,
) -> eyre::Result<L2BlockEvidence>
where
    P: StateProviderFactory,
    T: alloy_consensus::Transaction + alloy_consensus::transaction::TxHashRef,
    R: TxReceipt<Log = Log>,
{
    eyre::ensure!(
        transactions.len() == receipts.len(),
        "block {} has {} transactions but {} receipts",
        block.number,
        transactions.len(),
        receipts.len()
    );
    eyre::ensure!(
        transactions.len() == senders.len(),
        "block {} has {} transactions but {} senders",
        block.number,
        transactions.len(),
        senders.len()
    );

    let mut advance_tempo_calls = Vec::new();
    let mut event_collector = EventCollector::default();
    for (transaction_index, ((transaction, sender), receipt)) in
        transactions.iter().zip(senders).zip(receipts).enumerate()
    {
        if let Some(call) = calldata::extract(
            transaction_index,
            transaction,
            *sender,
            receipt,
            block.number,
        )? {
            advance_tempo_calls.push(call);
        }
        event_collector.extract_receipt(transaction_index, transaction, receipt, block.number)?;
    }
    let events = event_collector.finish(block.number)?;
    let tokens =
        token_enablement_candidates(&advance_tempo_calls, events.token_enabled_addresses());
    let token_states = state::read_token_enablement_state(provider, &tokens, block)?;

    Ok(L2BlockEvidence {
        block,
        advance_tempo_calls,
        events,
        token_states,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes};
    use tempo_zone_contracts::IZoneInbox;

    fn call(success: bool, tokens: &[Address]) -> L2AdvanceTempoEvidence {
        L2AdvanceTempoEvidence {
            transaction_hash: B256::ZERO,
            transaction_index: 0,
            sender: Address::ZERO,
            target: Address::ZERO,
            success,
            raw_input: Bytes::new(),
            header: Bytes::new(),
            deposits: vec![],
            decryptions: vec![],
            enabled_tokens: tokens
                .iter()
                .map(|token| IZoneInbox::EnabledToken {
                    token: *token,
                    name: String::new(),
                    symbol: String::new(),
                    currency: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn token_enablement_candidates_are_deduplicated_in_first_seen_order() {
        let a = Address::repeat_byte(1);
        let b = Address::repeat_byte(2);
        let c = Address::repeat_byte(3);
        let candidates =
            token_enablement_candidates(&[call(false, &[c]), call(true, &[a, b, a])], [b, c]);
        assert_eq!(candidates, vec![a, b, c]);
    }
}
