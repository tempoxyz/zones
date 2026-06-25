//! Privacy-enforced log filtering for the zone's private RPC.
//!
//! Only whitelisted TIP-20 event logs are returned to callers, and only when
//! the caller's address appears in an eligible indexed topic position for that
//! event type. This prevents users from observing other users' token activity.

use alloy_consensus::TxReceipt;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, b256};
use alloy_rpc_types_eth::{Filter, FilterSet, Log};
use tempo_alloy::rpc::TempoTransactionReceipt;

use crate::types::JsonRpcError;

/// `Transfer(address,address,uint256)`
pub const TRANSFER_TOPIC: B256 =
    b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// `Approval(address,address,uint256)`
pub const APPROVAL_TOPIC: B256 =
    b256!("0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925");

/// `TransferWithMemo(address,address,uint256,bytes32)`
pub const TRANSFER_WITH_MEMO_TOPIC: B256 =
    b256!("0x57bc7354aa85aed339e000bccffabbc529466af35f0772c8f8ee1145927de7f0");

/// `Mint(address,uint256)`
pub const MINT_TOPIC: B256 =
    b256!("0x0f6798a560793a54c3bcfe86a93cde1e73087d944c0ea20544137d4121396885");

/// `Burn(address,uint256)`
pub const BURN_TOPIC: B256 =
    b256!("0xcc16f5dbb4873280815c1ee09dbd06736cffcc184412cf7a71a0fdb75d397ca5");

/// All whitelisted TIP-20 event topic hashes.
pub const WHITELISTED_TOPICS: [B256; 5] = [
    TRANSFER_TOPIC,
    APPROVAL_TOPIC,
    TRANSFER_WITH_MEMO_TOPIC,
    MINT_TOPIC,
    BURN_TOPIC,
];

const TWO_PARTY_TOPICS: [B256; 3] = [TRANSFER_TOPIC, APPROVAL_TOPIC, TRANSFER_WITH_MEMO_TOPIC];
const CALLER_SCOPED_FILTER_ERROR: &str =
    "private log filter must include authenticated caller in topic1 or topic2";

/// Returns `true` if `caller` appears in an eligible indexed-topic position
/// for the log's event type.
///
/// Topic positions checked per event:
/// - **Transfer / TransferWithMemo**: topic1 (from) or topic2 (to)
/// - **Approval**: topic1 (owner) or topic2 (spender)
/// - **Mint**: topic1 (to)
/// - **Burn**: topic1 (from)
pub fn is_caller_eligible(log: &Log, caller: &Address) -> bool {
    let topics = log.topics();
    let topic0 = match topics.first() {
        Some(t) => t,
        None => return false,
    };

    let caller_word = B256::left_padding_from(caller.as_slice());

    if *topic0 == TRANSFER_TOPIC || *topic0 == APPROVAL_TOPIC || *topic0 == TRANSFER_WITH_MEMO_TOPIC
    {
        // topic1 or topic2 must match caller
        topics.get(1) == Some(&caller_word) || topics.get(2) == Some(&caller_word)
    } else if *topic0 == MINT_TOPIC || *topic0 == BURN_TOPIC {
        // topic1 must match caller
        topics.get(1) == Some(&caller_word)
    } else {
        false
    }
}

/// Filters logs to only those the caller is allowed to see.
///
/// A log is included only when **both** of the following hold:
/// 1. Its topic0 is one of the [`WHITELISTED_TOPICS`].
/// 2. The `caller` is eligible per [`is_caller_eligible`].
pub fn is_log_visible(log: &Log, caller: &Address) -> bool {
    log.topic0().is_some_and(|t| WHITELISTED_TOPICS.contains(t)) && is_caller_eligible(log, caller)
}

/// Filters logs to only those the caller is allowed to see.
pub fn filter_logs(logs: Vec<Log>, caller: &Address) -> Vec<Log> {
    logs.into_iter()
        .filter(|log| is_log_visible(log, caller))
        .collect()
}

/// Filters a receipt's logs for its sender and recomputes `logsBloom`.
pub fn filter_receipt_logs(mut receipt: TempoTransactionReceipt) -> TempoTransactionReceipt {
    let caller = receipt.from();
    let logs = core::mem::take(&mut receipt.inner.inner.receipt.logs);
    receipt.inner.inner.receipt.logs = filter_logs(logs, &caller);
    receipt.inner.inner.logs_bloom = receipt.inner.inner.receipt.bloom();
    receipt
}

/// Scopes a user-supplied filter to only match enabled zone token addresses.
pub fn scope_filter_addresses(
    filter: &mut Filter,
    zone_tokens: &[Address],
) -> Result<(), JsonRpcError> {
    let requested_addresses: Vec<Address> = filter.address.iter().copied().collect();

    if requested_addresses.is_empty() {
        filter.address = FilterSet::from(zone_tokens.to_vec());
        return Ok(());
    }

    if requested_addresses
        .iter()
        .all(|address| zone_tokens.contains(address))
    {
        Ok(())
    } else {
        Err(JsonRpcError::invalid_params("invalid filter address"))
    }
}

/// Scopes a user-supplied filter to only match whitelisted TIP-20 event topics.
///
/// Intersects the user's requested topic0 with [`WHITELISTED_TOPICS`].
/// If the user omitted topic0, restricts to the whitelisted set.
/// If the intersection is empty, sets topic0 to a dummy that will match nothing.
///
/// The post-filter in [`filter_logs`] remains the actual privacy enforcement;
/// this pre-filter reduces DB scan volume and timing side-channels.
pub fn scope_filter(filter: &mut Filter) {
    // --- Topic0 scoping ---
    let user_topic0: Vec<B256> = filter.topics[0].iter().copied().collect();

    let scoped_topic0: Vec<B256> = if user_topic0.is_empty() {
        // User didn't specify — restrict to whitelisted events
        WHITELISTED_TOPICS.to_vec()
    } else {
        // Intersect user's requested topics with whitelist
        user_topic0
            .into_iter()
            .filter(|t| WHITELISTED_TOPICS.contains(t))
            .collect()
    };

    if scoped_topic0.is_empty() {
        // No matching topics — use a dummy topic that will never match
        filter.topics[0] = FilterSet::from(B256::ZERO);
    } else {
        filter.topics[0] = FilterSet::from(scoped_topic0);
    }
}

/// Scopes a user-supplied filter to whitelisted event topics and requires the
/// authenticated caller to appear in an eligible indexed topic before backend
/// log retrieval.
pub fn scope_filter_for_caller(filter: &mut Filter, caller: &Address) -> Result<(), JsonRpcError> {
    scope_filter(filter);
    if filter.topics[0].len() == 1 && filter.topics[0].contains(&B256::ZERO) {
        return Ok(());
    }

    let caller_word = B256::left_padding_from(caller.as_slice());
    if filter.topics[1].contains(&caller_word) {
        filter.topics[1] = FilterSet::from(caller_word);
        return Ok(());
    }

    if filter.topics[2].contains(&caller_word) {
        let topic0 = filter.topics[0]
            .iter()
            .copied()
            .filter(|topic| TWO_PARTY_TOPICS.contains(topic))
            .collect::<Vec<_>>();
        if !topic0.is_empty() {
            filter.topics[0] = FilterSet::from(topic0);
            filter.topics[2] = FilterSet::from(caller_word);
            return Ok(());
        }
    }

    Err(JsonRpcError::invalid_params(CALLER_SCOPED_FILTER_ERROR))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::ReceiptWithBloom;
    use alloy_primitives::{Address, Bytes, LogData, TxHash, address, keccak256};
    use alloy_rpc_types_eth::TransactionReceipt;
    use tempo_alloy::rpc::TempoTransactionReceipt;
    use tempo_primitives::{TempoReceipt, TempoTxType};

    /// Build a test `Log` with the given emitting address and topics.
    fn make_log(emitter: Address, topics: Vec<B256>) -> Log {
        Log {
            inner: alloy_primitives::Log {
                address: emitter,
                data: LogData::new_unchecked(topics, Bytes::new()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    fn caller_word(addr: &Address) -> B256 {
        B256::left_padding_from(addr.as_slice())
    }

    fn make_receipt(from: Address, logs: Vec<Log>) -> TempoTransactionReceipt {
        let receipt = TempoReceipt {
            tx_type: TempoTxType::Legacy,
            success: true,
            cumulative_gas_used: 21_000,
            logs,
        };

        TempoTransactionReceipt {
            inner: TransactionReceipt {
                inner: ReceiptWithBloom::from(receipt),
                transaction_hash: TxHash::with_last_byte(1),
                transaction_index: Some(0),
                block_hash: Some(B256::with_last_byte(2)),
                block_number: Some(1),
                gas_used: 21_000,
                effective_gas_price: 1,
                blob_gas_used: None,
                blob_gas_price: None,
                from,
                to: Some(Address::ZERO),
                contract_address: None,
            },
            fee_token: None,
            fee_payer: from,
        }
    }

    // ---------------------------------------------------------------
    // Verify topic hashes match the Solidity event signatures
    // ---------------------------------------------------------------

    #[test]
    fn topic_hashes_match_signatures() {
        assert_eq!(
            TRANSFER_TOPIC,
            keccak256(b"Transfer(address,address,uint256)")
        );
        assert_eq!(
            APPROVAL_TOPIC,
            keccak256(b"Approval(address,address,uint256)")
        );
        assert_eq!(
            TRANSFER_WITH_MEMO_TOPIC,
            keccak256(b"TransferWithMemo(address,address,uint256,bytes32)")
        );
        assert_eq!(MINT_TOPIC, keccak256(b"Mint(address,uint256)"));
        assert_eq!(BURN_TOPIC, keccak256(b"Burn(address,uint256)"));
    }

    // ---------------------------------------------------------------
    // is_caller_eligible — Transfer
    // ---------------------------------------------------------------

    #[test]
    fn transfer_eligible_as_sender() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let log = make_log(
            Address::ZERO,
            vec![TRANSFER_TOPIC, caller_word(&caller), caller_word(&other)],
        );
        assert!(is_caller_eligible(&log, &caller));
    }

    #[test]
    fn transfer_eligible_as_receiver() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let log = make_log(
            Address::ZERO,
            vec![TRANSFER_TOPIC, caller_word(&other), caller_word(&caller)],
        );
        assert!(is_caller_eligible(&log, &caller));
    }

    #[test]
    fn transfer_rejected_when_not_participant() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let a = address!("0x0000000000000000000000000000000000000002");
        let b = address!("0x0000000000000000000000000000000000000003");
        let log = make_log(
            Address::ZERO,
            vec![TRANSFER_TOPIC, caller_word(&a), caller_word(&b)],
        );
        assert!(!is_caller_eligible(&log, &caller));
    }

    // ---------------------------------------------------------------
    // is_caller_eligible — Approval
    // ---------------------------------------------------------------

    #[test]
    fn approval_eligible_as_owner() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let spender = address!("0x0000000000000000000000000000000000000002");
        let log = make_log(
            Address::ZERO,
            vec![APPROVAL_TOPIC, caller_word(&caller), caller_word(&spender)],
        );
        assert!(is_caller_eligible(&log, &caller));
    }

    #[test]
    fn approval_eligible_as_spender() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let owner = address!("0x0000000000000000000000000000000000000002");
        let log = make_log(
            Address::ZERO,
            vec![APPROVAL_TOPIC, caller_word(&owner), caller_word(&caller)],
        );
        assert!(is_caller_eligible(&log, &caller));
    }

    #[test]
    fn approval_rejected_when_not_participant() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let a = address!("0x0000000000000000000000000000000000000002");
        let b = address!("0x0000000000000000000000000000000000000003");
        let log = make_log(
            Address::ZERO,
            vec![APPROVAL_TOPIC, caller_word(&a), caller_word(&b)],
        );
        assert!(!is_caller_eligible(&log, &caller));
    }

    // ---------------------------------------------------------------
    // is_caller_eligible — TransferWithMemo
    // ---------------------------------------------------------------

    #[test]
    fn transfer_with_memo_eligible_as_sender() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let log = make_log(
            Address::ZERO,
            vec![
                TRANSFER_WITH_MEMO_TOPIC,
                caller_word(&caller),
                caller_word(&other),
            ],
        );
        assert!(is_caller_eligible(&log, &caller));
    }

    #[test]
    fn transfer_with_memo_eligible_as_receiver() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let log = make_log(
            Address::ZERO,
            vec![
                TRANSFER_WITH_MEMO_TOPIC,
                caller_word(&other),
                caller_word(&caller),
            ],
        );
        assert!(is_caller_eligible(&log, &caller));
    }

    #[test]
    fn transfer_with_memo_rejected_when_not_participant() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let a = address!("0x0000000000000000000000000000000000000002");
        let b = address!("0x0000000000000000000000000000000000000003");
        let log = make_log(
            Address::ZERO,
            vec![TRANSFER_WITH_MEMO_TOPIC, caller_word(&a), caller_word(&b)],
        );
        assert!(!is_caller_eligible(&log, &caller));
    }

    // ---------------------------------------------------------------
    // is_caller_eligible — Mint / Burn
    // ---------------------------------------------------------------

    #[test]
    fn mint_eligible_as_recipient() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let log = make_log(Address::ZERO, vec![MINT_TOPIC, caller_word(&caller)]);
        assert!(is_caller_eligible(&log, &caller));
    }

    #[test]
    fn mint_rejected_when_not_recipient() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let log = make_log(Address::ZERO, vec![MINT_TOPIC, caller_word(&other)]);
        assert!(!is_caller_eligible(&log, &caller));
    }

    #[test]
    fn burn_eligible_as_burner() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let log = make_log(Address::ZERO, vec![BURN_TOPIC, caller_word(&caller)]);
        assert!(is_caller_eligible(&log, &caller));
    }

    #[test]
    fn burn_rejected_when_not_burner() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let log = make_log(Address::ZERO, vec![BURN_TOPIC, caller_word(&other)]);
        assert!(!is_caller_eligible(&log, &caller));
    }

    // ---------------------------------------------------------------
    // is_caller_eligible — unknown / empty topic
    // ---------------------------------------------------------------

    #[test]
    fn unknown_topic_rejected() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let unknown = B256::with_last_byte(0xff);
        let log = make_log(Address::ZERO, vec![unknown, caller_word(&caller)]);
        assert!(!is_caller_eligible(&log, &caller));
    }

    #[test]
    fn empty_topics_rejected() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let log = make_log(Address::ZERO, vec![]);
        assert!(!is_caller_eligible(&log, &caller));
    }

    // ---------------------------------------------------------------
    // filter_logs
    // ---------------------------------------------------------------

    #[test]
    fn filter_logs_keeps_eligible_and_drops_others() {
        let zone_token = address!("0x000000000000000000000000000000000000aaaa");
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");

        let eligible = make_log(
            zone_token,
            vec![TRANSFER_TOPIC, caller_word(&caller), caller_word(&other)],
        );
        let wrong_topic = make_log(
            zone_token,
            vec![B256::with_last_byte(0x01), caller_word(&caller)],
        );
        let not_eligible = make_log(
            zone_token,
            vec![TRANSFER_TOPIC, caller_word(&other), caller_word(&other)],
        );

        let logs = vec![eligible.clone(), wrong_topic, not_eligible];
        let result = filter_logs(logs, &caller);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], eligible);
    }

    #[test]
    fn filter_logs_empty_input() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let result = filter_logs(vec![], &caller);
        assert!(result.is_empty());
    }

    #[test]
    fn filter_receipt_logs_recomputes_logs_and_bloom() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let third = address!("0x0000000000000000000000000000000000000003");
        let hidden_topic = keccak256(b"PolicyUpdated(address,uint256)");

        let visible = make_log(
            Address::ZERO,
            vec![TRANSFER_TOPIC, caller_word(&caller), caller_word(&other)],
        );
        let hidden_transfer = make_log(
            Address::ZERO,
            vec![TRANSFER_TOPIC, caller_word(&other), caller_word(&third)],
        );
        let hidden_event = make_log(Address::ZERO, vec![hidden_topic, caller_word(&caller)]);

        let filtered = filter_receipt_logs(make_receipt(
            caller,
            vec![
                visible.clone(),
                hidden_transfer.clone(),
                hidden_event.clone(),
            ],
        ));

        assert_eq!(filtered.inner.logs(), std::slice::from_ref(&visible));
        assert_eq!(
            filtered.inner.inner.logs_bloom,
            alloy_primitives::logs_bloom(filtered.inner.logs().iter().map(|log| log.as_ref())),
        );
        assert_ne!(
            filtered.inner.inner.logs_bloom,
            alloy_primitives::logs_bloom(
                [visible, hidden_transfer, hidden_event]
                    .iter()
                    .map(|log| log.as_ref())
            ),
        );
    }

    // ---------------------------------------------------------------
    // scope_filter
    // ---------------------------------------------------------------

    #[test]
    fn scope_filter_scopes_topic0() {
        let mut filter = Filter::default();
        scope_filter(&mut filter);
        for topic in &WHITELISTED_TOPICS {
            assert!(filter.topics[0].contains(topic));
        }
        assert_eq!(filter.topics[0].len(), WHITELISTED_TOPICS.len());
    }

    #[test]
    fn scope_filter_intersects_topic0() {
        let bogus_topic = B256::with_last_byte(0xff);
        let mut filter = Filter::default();
        filter.topics[0] = FilterSet::from(vec![TRANSFER_TOPIC, bogus_topic]);
        scope_filter(&mut filter);
        assert!(filter.topics[0].contains(&TRANSFER_TOPIC));
        assert!(!filter.topics[0].contains(&bogus_topic));
        assert_eq!(filter.topics[0].len(), 1);
    }

    #[test]
    fn scope_filter_empty_intersection() {
        let bogus = B256::with_last_byte(0xff);
        let mut filter = Filter::default();
        filter.topics[0] = FilterSet::from(bogus);
        scope_filter(&mut filter);
        assert_eq!(filter.topics[0], FilterSet::from(B256::ZERO));
    }

    #[test]
    fn scope_filter_for_caller_rejects_broad_filter() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let mut filter = Filter::default();

        let err = scope_filter_for_caller(&mut filter, &caller).unwrap_err();

        assert_eq!(err.code, JsonRpcError::invalid_params("").code);
        assert_eq!(err.message, CALLER_SCOPED_FILTER_ERROR);
    }

    #[test]
    fn scope_filter_for_caller_scopes_topic1_caller() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let caller_topic = caller_word(&caller);
        let other_topic = caller_word(&other);
        let mut filter = Filter::default();
        filter.topics[1] = FilterSet::from(vec![caller_topic, other_topic]);
        filter.topics[2] = FilterSet::from(other_topic);

        scope_filter_for_caller(&mut filter, &caller).unwrap();

        assert_eq!(filter.topics[0].len(), WHITELISTED_TOPICS.len());
        assert_eq!(filter.topics[1], FilterSet::from(caller_topic));
        assert_eq!(filter.topics[2], FilterSet::from(other_topic));
    }

    #[test]
    fn scope_filter_for_caller_scopes_topic2_caller_for_two_party_events() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let caller_topic = caller_word(&caller);
        let other_topic = caller_word(&other);
        let mut filter = Filter::default();
        filter.topics[0] = FilterSet::from(vec![TRANSFER_TOPIC, MINT_TOPIC]);
        filter.topics[1] = FilterSet::from(other_topic);
        filter.topics[2] = FilterSet::from(vec![caller_topic, caller_word(&other)]);

        scope_filter_for_caller(&mut filter, &caller).unwrap();

        assert_eq!(filter.topics[0], FilterSet::from(TRANSFER_TOPIC));
        assert_eq!(filter.topics[1], FilterSet::from(other_topic));
        assert_eq!(filter.topics[2], FilterSet::from(caller_topic));
    }

    #[test]
    fn scope_filter_for_caller_rejects_wrong_caller() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let a = address!("0x0000000000000000000000000000000000000002");
        let b = address!("0x0000000000000000000000000000000000000003");
        let mut filter = Filter::default();
        filter.topics[0] = FilterSet::from(TRANSFER_TOPIC);
        filter.topics[1] = FilterSet::from(caller_word(&a));
        filter.topics[2] = FilterSet::from(caller_word(&b));

        let err = scope_filter_for_caller(&mut filter, &caller).unwrap_err();

        assert_eq!(err.code, JsonRpcError::invalid_params("").code);
        assert_eq!(err.message, CALLER_SCOPED_FILTER_ERROR);
    }

    #[test]
    fn scope_filter_for_caller_rejects_topic2_only_for_one_party_events() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let mut filter = Filter::default();
        filter.topics[0] = FilterSet::from(MINT_TOPIC);
        filter.topics[2] = FilterSet::from(caller_word(&caller));

        let err = scope_filter_for_caller(&mut filter, &caller).unwrap_err();

        assert_eq!(err.code, JsonRpcError::invalid_params("").code);
        assert_eq!(err.message, CALLER_SCOPED_FILTER_ERROR);
    }

    #[test]
    fn scope_filter_addresses_scopes_omitted_address() {
        let token_a = address!("0x00000000000000000000000000000000000000aa");
        let token_b = address!("0x00000000000000000000000000000000000000bb");
        let mut filter = Filter::default();

        scope_filter_addresses(&mut filter, &[token_a, token_b]).unwrap();

        assert!(filter.address.contains(&token_a));
        assert!(filter.address.contains(&token_b));
        assert_eq!(filter.address.len(), 2);
    }

    #[test]
    fn scope_filter_addresses_allows_enabled_token_address() {
        let token = address!("0x00000000000000000000000000000000000000aa");
        let mut filter = Filter {
            address: FilterSet::from(token),
            ..Default::default()
        };

        scope_filter_addresses(&mut filter, &[token]).unwrap();

        assert_eq!(filter.address, FilterSet::from(token));
    }

    #[test]
    fn scope_filter_addresses_rejects_non_zone_token_address() {
        let token = address!("0x00000000000000000000000000000000000000aa");
        let other = address!("0x00000000000000000000000000000000000000cc");
        let mut filter = Filter {
            address: FilterSet::from(vec![token, other]),
            ..Default::default()
        };

        let err = scope_filter_addresses(&mut filter, &[token]).unwrap_err();

        assert_eq!(err.code, JsonRpcError::invalid_params("").code);
        assert_eq!(err.message, "invalid filter address");
    }
}
