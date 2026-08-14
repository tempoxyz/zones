//! Ordered classification of authenticated receipt logs.

use alloy_network::ReceiptResponse as _;
use alloy_primitives::{Address, B256};
use tempo_alloy::rpc::TempoTransactionReceipt;

use crate::observe::events::{L1ProtocolEvent, Portal, classify_l1_protocol_event};

use super::OrderedL1Outcome;
use crate::observe::error::{ObservationError, PortalCallFamily, ProtocolChain};

/// Protocol outcomes from one successful transaction before its calldata is reconciled.
pub(super) struct PendingTransaction {
    pub(super) transaction_index: usize,
    pub(super) transaction_hash: B256,
    pub(super) required_calls: Vec<PortalCallFamily>,
    pub(super) outcomes: Vec<OrderedL1Outcome>,
}

/// Classify successful receipt logs in block order and derive their calldata requirement.
pub(super) fn ordered_transactions(
    portal: Address,
    transaction_hashes: &[B256],
    receipts: &[TempoTransactionReceipt],
) -> Result<Vec<PendingTransaction>, ObservationError> {
    debug_assert_eq!(transaction_hashes.len(), receipts.len());

    let mut transactions = Vec::new();
    let mut block_log_index = 0usize;
    for (transaction_index, (transaction_hash, receipt)) in
        transaction_hashes.iter().zip(receipts).enumerate()
    {
        if !receipt.status() {
            continue;
        }

        let mut required_calls = Vec::new();
        let mut outcomes = Vec::new();
        for (receipt_log_index, log) in receipt.logs().iter().enumerate() {
            let log_index = block_log_index;
            block_log_index += 1;

            let Some(event) = classify_l1_protocol_event(portal, &log.inner).map_err(|error| {
                ObservationError::protocol_event(
                    ProtocolChain::TempoL1,
                    transaction_index,
                    receipt_log_index,
                    log_index,
                    *transaction_hash,
                    error,
                )
            })?
            else {
                continue;
            };
            if matches!(event, L1ProtocolEvent::KnownIgnored) {
                continue;
            }

            if let Some(required) = call_requirement(&event)
                && !required_calls.contains(&required)
            {
                required_calls.push(required);
            }
            outcomes.push(OrderedL1Outcome { event });
        }

        if !outcomes.is_empty() {
            transactions.push(PendingTransaction {
                transaction_index,
                transaction_hash: *transaction_hash,
                required_calls,
                outcomes,
            });
        }
    }
    Ok(transactions)
}

/// Return the single top-level Portal family an event requires, if any.
fn call_requirement(event: &L1ProtocolEvent) -> Option<PortalCallFamily> {
    match event {
        L1ProtocolEvent::Portal(Portal::ZonePortalEvents::BatchSubmitted(_)) => {
            Some(PortalCallFamily::SubmitBatch)
        }
        L1ProtocolEvent::Portal(
            Portal::ZonePortalEvents::WithdrawalProcessed(_)
            | Portal::ZonePortalEvents::WithdrawalBounceBack(_)
            | Portal::ZonePortalEvents::DepositBounceBack(_)
            | Portal::ZonePortalEvents::DepositBounceBackPending(_),
        ) => Some(PortalCallFamily::ProcessWithdrawals),
        L1ProtocolEvent::Portal(
            Portal::ZonePortalEvents::DepositMade(_)
            | Portal::ZonePortalEvents::TokenEnabled(_)
            | Portal::ZonePortalEvents::RefundClaimed(_)
            | Portal::ZonePortalEvents::BouncebackGasUpdated(_),
        )
        | L1ProtocolEvent::Portal(_)
        | L1ProtocolEvent::FactoryZoneCreated(_)
        | L1ProtocolEvent::KnownIgnored => None,
    }
}
