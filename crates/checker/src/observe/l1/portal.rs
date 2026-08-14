//! Direct-Portal calldata decoding from authenticated block envelopes.

use alloy_primitives::{Address, B256, TxKind};
use alloy_sol_types::SolCall;
use tempo_primitives::TempoTxEnvelope;
use tempo_zone_contracts::ZonePortal;

use super::super::{
    abi::{DecodedPortalCall, decode_portal_call},
    error::{
        AuthenticatedTransaction, ObservationError, PortalCallError, PortalCallFamily,
        ProtocolChain,
    },
};

/// Decode relevant direct Portal calls in authenticated top-level execution order.
pub(super) fn decode_direct_portal_calls(
    envelope: &TempoTxEnvelope,
    portal: Address,
    transaction_index: usize,
    transaction_hash: B256,
    required: &[PortalCallFamily],
) -> Result<Vec<DecodedPortalCall>, ObservationError> {
    let coordinate =
        AuthenticatedTransaction::new(ProtocolChain::TempoL1, transaction_index, transaction_hash);
    let first_target = envelope.calls().next().and_then(|(kind, _)| match kind {
        TxKind::Call(target) => Some(target),
        TxKind::Create => None,
    });
    let mut calls = Vec::new();
    let mut other_family = None;
    let mut saw_empty_required_process = false;
    for (kind, calldata) in envelope.calls() {
        if kind != TxKind::Call(portal) {
            continue;
        }
        let family = if calldata.starts_with(&ZonePortal::submitBatchCall::SELECTOR) {
            PortalCallFamily::SubmitBatch
        } else if calldata.starts_with(&ZonePortal::processWithdrawalsCall::SELECTOR) {
            PortalCallFamily::ProcessWithdrawals
        } else {
            continue;
        };
        if !required.contains(&family) {
            other_family.get_or_insert(family);
            continue;
        }
        let decoded = decode_portal_call(calldata, coordinate)?;
        if family == PortalCallFamily::ProcessWithdrawals
            && !decoded.is_nonempty_process_withdrawals()
        {
            saw_empty_required_process = true;
            continue;
        }
        calls.push(decoded);
    }
    for expected in required {
        if !calls.iter().any(|call| call.family() == *expected) {
            if *expected == PortalCallFamily::ProcessWithdrawals && saw_empty_required_process {
                return Err(PortalCallError::EmptyProcessWithOutcomes { transaction_hash }.into());
            }
            if let Some(actual) = other_family {
                return Err(PortalCallError::FamilyMismatch {
                    transaction_hash,
                    expected: *expected,
                    actual,
                }
                .into());
            }
            return Err(PortalCallError::UnsupportedNestedPortalCall {
                transaction_hash,
                target: first_target,
            }
            .into());
        }
    }
    Ok(calls)
}
