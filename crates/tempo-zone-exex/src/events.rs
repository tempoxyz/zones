//! Event parsing utilities for extracting withdrawals from block receipts.

use crate::types::{Withdrawal, WithdrawalRequested};
use alloy_primitives::{Address, Log, U128};
use alloy_sol_types::SolEvent;

/// Parses WithdrawalRequested logs from block receipts.
///
/// Filters logs by the WithdrawalRequested event signature and the outbox contract address,
/// then decodes each matching log into a structured event.
pub fn parse_withdrawal_logs(
    logs: &[Log],
    outbox_address: Address,
) -> Vec<(u64, WithdrawalRequested)> {
    logs.iter()
        .filter(|log| log.address == outbox_address)
        .filter_map(|log| {
            if log.topics().first() != Some(&WithdrawalRequested::SIGNATURE_HASH) {
                return None;
            }
            let decoded = WithdrawalRequested::decode_log(log).ok()?;
            Some((decoded.data.withdrawalIndex, decoded.data))
        })
        .collect()
}

/// Extracts a Withdrawal struct from a decoded WithdrawalRequested event.
pub fn withdrawal_from_event(event: &WithdrawalRequested) -> Withdrawal {
    Withdrawal {
        sender: event.sender,
        to: event.to,
        amount: U128::from(event.amount),
        memo: event.memo,
        gas_limit: event.gasLimit,
        fallback_recipient: event.fallbackRecipient,
        data: event.data.clone(),
    }
}

/// Extracts all withdrawals from a list of logs.
///
/// Convenience function that combines parsing and extraction.
pub fn extract_withdrawals(logs: &[Log], outbox_address: Address) -> Vec<Withdrawal> {
    parse_withdrawal_logs(logs, outbox_address)
        .into_iter()
        .map(|(_, event)| withdrawal_from_event(&event))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, address, b256};
    use alloy_sol_types::SolEvent;

    #[test]
    fn test_withdrawal_requested_signature() {
        assert_eq!(
            WithdrawalRequested::SIGNATURE_HASH,
            b256!("4e42c826452af660647013f93fcfb200cc119e1c201c7b12bfbdec33bb5e6218")
        );
    }

    #[test]
    fn test_extract_withdrawals_filters_by_address() {
        let outbox = address!("1111111111111111111111111111111111111111");
        let other = address!("2222222222222222222222222222222222222222");

        let logs = vec![
            Log::new(other, vec![WithdrawalRequested::SIGNATURE_HASH], Bytes::new()).unwrap(),
        ];

        let withdrawals = extract_withdrawals(&logs, outbox);
        assert!(withdrawals.is_empty());
    }
}
