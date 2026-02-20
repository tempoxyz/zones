//! Privacy-enforced log filtering for the zone's private RPC.
//!
//! Only whitelisted TIP-20 event logs are returned to callers, and only when
//! the caller's address appears in an eligible indexed topic position for that
//! event type. This prevents users from observing other users' token activity.

use alloy_primitives::{Address, B256, b256};
use alloy_rpc_types_eth::{Filter, FilterSet, Log};

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
///
/// TODO: once the enabled-token registry is plumbed through, also filter
/// by emitting contract address (only logs from enabled TIP-20 tokens).
pub fn filter_logs(logs: Vec<Log>, caller: &Address) -> Vec<Log> {
    logs.into_iter()
        .filter(|log| {
            log.topic0().is_some_and(|t| WHITELISTED_TOPICS.contains(t))
                && is_caller_eligible(log, caller)
        })
        .collect()
}

/// Scopes a user-supplied filter to only match whitelisted TIP-20 event topics.
///
/// Intersects the user's requested topic0 with [`WHITELISTED_TOPICS`].
/// If the user omitted topic0, restricts to the whitelisted set.
/// If the intersection is empty, sets topic0 to a dummy that will match nothing.
///
/// The post-filter in [`filter_logs`] remains the actual privacy enforcement;
/// this pre-filter reduces DB scan volume and timing side-channels.
///
/// TODO: once the enabled-token registry is plumbed through, also scope the
/// filter's `address` field to only match enabled TIP-20 token addresses.
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, LogData, address, keccak256};

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
}
