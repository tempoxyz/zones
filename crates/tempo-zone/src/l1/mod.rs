//! L1 chain subscription and deposit extraction.

mod deposits;
pub use deposits::{Deposit, EncryptedDeposit, L1BlockDeposits, L1Deposit, PreparedL1Block};

mod queue;
pub use queue::DepositQueue;
#[cfg(test)]
use queue::PendingDeposits;

mod subscriber;
pub(crate) use subscriber::EnqueueOutcome;
pub use subscriber::{
    EnabledToken, L1PortalEvents, L1SequencerEvent, L1Subscriber, L1SubscriberConfig,
};

#[cfg(test)]
mod tests {
    use super::{
        subscriber::{
            LocalTempoStateReader, address_to_storage_value, apply_sequencer_events_to_cache,
        },
        *,
    };
    use crate::{
        abi::{
            self, DepositType, PORTAL_PENDING_SEQUENCER_SLOT, PORTAL_SEQUENCER_SLOT,
            ZonePortal::{BounceBack, SequencerTransferStarted, SequencerTransferred},
        },
        l1_state::{cache::L1StateCache, tip403::PolicyEvent},
    };
    use alloy_consensus::{BlockHeader as _, Header};
    use alloy_primitives::{Address, B256, FixedBytes, U256, address, keccak256};
    use alloy_provider::ProviderBuilder;
    use alloy_rpc_types_eth::Log;
    use alloy_sol_types::{SolEvent, SolValue};
    use alloy_transport::mock::Asserter;
    use parking_lot::Mutex;
    use reth_primitives_traits::SealedHeader;
    use std::{
        collections::{HashSet, VecDeque},
        sync::Arc,
        time::Duration,
    };
    use tempo_alloy::TempoNetwork;
    use tempo_primitives::TempoHeader;

    struct SequenceLocalTempoStateReader {
        values: Mutex<VecDeque<u64>>,
        last_value: u64,
    }

    impl SequenceLocalTempoStateReader {
        fn new(values: impl Into<VecDeque<u64>>) -> Self {
            let values = values.into();
            let last_value = values.back().copied().unwrap_or_default();
            Self {
                values: Mutex::new(values),
                last_value,
            }
        }
    }

    impl LocalTempoStateReader for SequenceLocalTempoStateReader {
        fn latest_tempo_block_number(&self) -> eyre::Result<u64> {
            let mut values = self.values.lock();
            Ok(values.pop_front().unwrap_or(self.last_value))
        }
    }

    fn test_subscriber(
        local_state: Arc<dyn LocalTempoStateReader>,
        genesis_tempo_block_number: Option<u64>,
    ) -> L1Subscriber {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");

        L1Subscriber {
            config: L1SubscriberConfig {
                l1_rpc_url: "http://127.0.0.1:8545".to_owned(),
                portal_address,
                genesis_tempo_block_number,
                policy_cache: crate::SharedPolicyCache::default(),
                l1_state_cache: crate::SharedL1StateCache::new(HashSet::from([portal_address])),
                l1_fetch_concurrency: 1,
                retry_connection_interval: Duration::from_secs(1),
            },
            local_state,
            deposit_queue: DepositQueue::default(),
            tracked_tokens: vec![],
            tip403_metrics: Default::default(),
            subscriber_metrics: Default::default(),
        }
    }

    fn make_test_header(number: u64) -> TempoHeader {
        TempoHeader {
            inner: Header {
                number,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Create a header that chains to the given parent.
    fn make_chained_header(number: u64, parent_hash: B256) -> TempoHeader {
        TempoHeader {
            inner: Header {
                number,
                parent_hash,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn seal(header: TempoHeader) -> SealedHeader<TempoHeader> {
        SealedHeader::seal_slow(header)
    }

    fn header_hash(header: &TempoHeader) -> B256 {
        keccak256(alloy_rlp::encode(header))
    }

    #[test]
    fn update_l1_state_anchor_reorg_clears_stale_policy_state() {
        use crate::l1_state::tip403::AuthRole;
        use tempo_contracts::precompiles::ITIP403Registry::PolicyType;

        let subscriber =
            test_subscriber(Arc::new(SequenceLocalTempoStateReader::new([0])), Some(0));
        let token = address!("0x0000000000000000000000000000000000000011");
        let user = address!("0x0000000000000000000000000000000000000022");

        let old_header = make_test_header(10);
        let old_hash = header_hash(&old_header);
        subscriber.update_l1_state_anchor(10, old_hash, old_header.inner.parent_hash);
        {
            let mut cache = subscriber.config.policy_cache.write();
            cache.set_token_policy(token, 10, 2);
            cache.set_policy_type(2, PolicyType::WHITELIST);
            cache.set_member(2, user, 10, true);
            cache.advance(10);
        }

        let replacement_parent = B256::with_last_byte(0x44);
        let replacement_header = make_chained_header(11, replacement_parent);
        subscriber.update_l1_state_anchor(11, header_hash(&replacement_header), replacement_parent);
        subscriber.apply_policy_events(
            11,
            &[
                PolicyEvent::TokenPolicyChanged {
                    token,
                    policy_id: 3,
                },
                PolicyEvent::PolicyCreated {
                    policy_id: 3,
                    policy_type: PolicyType::WHITELIST,
                },
                PolicyEvent::MembershipChanged {
                    policy_id: 3,
                    account: user,
                    in_set: false,
                },
            ],
        );

        let cache = subscriber.config.policy_cache.read();
        assert!(cache.policies().get(&2).is_none());
        assert_eq!(
            cache.is_authorized(token, user, 11, AuthRole::Transfer),
            Some(false)
        );
    }

    /// Confirm the front of the queue, panicking if it fails.
    fn confirm(queue: &mut PendingDeposits) -> L1BlockDeposits {
        let num_hash = queue.peek().expect("queue is empty").header.num_hash();
        queue.confirm(num_hash).expect("confirm mismatch")
    }

    /// Confirm the front of a shared `DepositQueue`, panicking if it fails.
    fn confirm_shared(queue: &DepositQueue) -> L1BlockDeposits {
        let num_hash = queue.peek().expect("queue is empty").header.num_hash();
        queue.confirm(num_hash).expect("confirm mismatch")
    }

    fn make_portal_log<E: SolEvent>(portal_address: Address, event: E) -> Log {
        Log {
            inner: alloy_primitives::Log {
                address: portal_address,
                data: event.encode_log_data(),
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

    #[tokio::test]
    async fn test_resolve_start_block_reads_live_local_state_each_time() {
        let subscriber = test_subscriber(
            Arc::new(SequenceLocalTempoStateReader::new(VecDeque::from([10, 11]))),
            None,
        );
        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(Asserter::new());

        assert_eq!(
            subscriber.resolve_start_block(&l1_provider).await.unwrap(),
            Some(11)
        );
        assert_eq!(
            subscriber.resolve_start_block(&l1_provider).await.unwrap(),
            Some(12)
        );
    }

    #[tokio::test]
    async fn test_resolve_start_block_falls_back_to_genesis_override_when_local_state_is_zero() {
        let subscriber = test_subscriber(
            Arc::new(SequenceLocalTempoStateReader::new(VecDeque::from([0]))),
            Some(42),
        );
        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(Asserter::new());

        assert_eq!(
            subscriber.resolve_start_block(&l1_provider).await.unwrap(),
            Some(43)
        );
    }

    #[test]
    fn test_push_log_decodes_bounce_back_as_regular_deposit() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let fallback_recipient = address!("0x00000000000000000000000000000000000000F1");
        let token = address!("0x0000000000000000000000000000000000002000");
        let event = BounceBack {
            newCurrentDepositQueueHash: B256::with_last_byte(0x42),
            fallbackRecipient: fallback_recipient,
            token,
            amount: 123_456,
            depositNumber: 1,
        };
        let log = Log {
            inner: alloy_primitives::Log {
                address: portal_address,
                data: event.encode_log_data(),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };

        let mut events = L1PortalEvents::default();
        events
            .push_log(&log, 123)
            .expect("bounce-back should decode");

        assert_eq!(events.deposits.len(), 1, "should enqueue one deposit");
        let L1Deposit::Regular(deposit) = &events.deposits[0] else {
            panic!("bounce-back should be mapped to a regular deposit");
        };
        assert_eq!(deposit.token, token);
        assert_eq!(deposit.sender, portal_address);
        assert_eq!(deposit.to, fallback_recipient);
        assert_eq!(deposit.amount, event.amount);
        assert_eq!(deposit.fee, 0, "bounce-back deposits should be fee-free");
        assert_eq!(
            deposit.memo,
            B256::ZERO,
            "bounce-back deposits should clear memo"
        );
    }

    #[test]
    fn test_push_log_decodes_sequencer_transfer_started() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let current_sequencer = address!("0x00000000000000000000000000000000000000A1");
        let pending_sequencer = address!("0x00000000000000000000000000000000000000B2");
        let event = SequencerTransferStarted {
            currentSequencer: current_sequencer,
            pendingSequencer: pending_sequencer,
        };
        let log = make_portal_log(portal_address, event);

        let mut events = L1PortalEvents::default();
        events
            .push_log(&log, 123)
            .expect("sequencer transfer start should decode");

        assert_eq!(
            events.sequencer_events,
            vec![L1SequencerEvent::TransferStarted {
                current_sequencer,
                pending_sequencer,
            }]
        );
        assert!(events.deposits.is_empty());
        assert!(events.enabled_tokens.is_empty());
    }

    #[test]
    fn test_push_log_decodes_sequencer_transferred() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let previous_sequencer = address!("0x00000000000000000000000000000000000000A1");
        let new_sequencer = address!("0x00000000000000000000000000000000000000B2");
        let event = SequencerTransferred {
            previousSequencer: previous_sequencer,
            newSequencer: new_sequencer,
        };
        let log = make_portal_log(portal_address, event);

        let mut events = L1PortalEvents::default();
        events
            .push_log(&log, 123)
            .expect("sequencer transferred should decode");

        assert_eq!(
            events.sequencer_events,
            vec![L1SequencerEvent::Transferred {
                previous_sequencer,
                new_sequencer,
            }]
        );
        assert!(events.deposits.is_empty());
        assert!(events.enabled_tokens.is_empty());
    }

    #[test]
    fn test_apply_sequencer_events_to_cache_sets_pending_sequencer() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let current_sequencer = address!("0x00000000000000000000000000000000000000A1");
        let pending_sequencer = address!("0x00000000000000000000000000000000000000B2");
        let mut cache = L1StateCache::new(HashSet::from([portal_address]));

        apply_sequencer_events_to_cache(
            &mut cache,
            portal_address,
            42,
            &[L1SequencerEvent::TransferStarted {
                current_sequencer,
                pending_sequencer,
            }],
        );

        assert_eq!(
            cache.get(portal_address, PORTAL_SEQUENCER_SLOT, 42),
            Some(address_to_storage_value(current_sequencer))
        );
        assert_eq!(
            cache.get(portal_address, PORTAL_PENDING_SEQUENCER_SLOT, 42),
            Some(address_to_storage_value(pending_sequencer))
        );
    }

    #[test]
    fn test_apply_sequencer_events_to_cache_accept_clears_pending_sequencer() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let previous_sequencer = address!("0x00000000000000000000000000000000000000A1");
        let new_sequencer = address!("0x00000000000000000000000000000000000000B2");
        let mut cache = L1StateCache::new(HashSet::from([portal_address]));

        apply_sequencer_events_to_cache(
            &mut cache,
            portal_address,
            43,
            &[L1SequencerEvent::Transferred {
                previous_sequencer,
                new_sequencer,
            }],
        );

        assert_eq!(
            cache.get(portal_address, PORTAL_SEQUENCER_SLOT, 43),
            Some(address_to_storage_value(new_sequencer))
        );
        assert_eq!(
            cache.get(portal_address, PORTAL_PENDING_SEQUENCER_SLOT, 43),
            Some(B256::ZERO)
        );
    }

    #[test]
    fn test_apply_sequencer_events_to_cache_preserves_in_block_event_order() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let sequencer_a = address!("0x00000000000000000000000000000000000000A1");
        let sequencer_b = address!("0x00000000000000000000000000000000000000B2");
        let sequencer_c = address!("0x00000000000000000000000000000000000000C3");
        let mut cache = L1StateCache::new(HashSet::from([portal_address]));

        apply_sequencer_events_to_cache(
            &mut cache,
            portal_address,
            44,
            &[
                L1SequencerEvent::TransferStarted {
                    current_sequencer: sequencer_a,
                    pending_sequencer: sequencer_b,
                },
                L1SequencerEvent::Transferred {
                    previous_sequencer: sequencer_a,
                    new_sequencer: sequencer_b,
                },
                L1SequencerEvent::TransferStarted {
                    current_sequencer: sequencer_b,
                    pending_sequencer: sequencer_c,
                },
            ],
        );

        assert_eq!(
            cache.get(portal_address, PORTAL_SEQUENCER_SLOT, 44),
            Some(address_to_storage_value(sequencer_b))
        );
        assert_eq!(
            cache.get(portal_address, PORTAL_PENDING_SEQUENCER_SLOT, 44),
            Some(address_to_storage_value(sequencer_c))
        );
    }

    #[test]
    fn test_deposit_queue_hash_chain() {
        let mut queue = PendingDeposits::default();
        assert_eq!(queue.enqueued_head_hash(), B256::ZERO);

        let d1 = L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000,
            fee: 0,
            memo: B256::ZERO,
        });

        queue.enqueue(
            make_test_header(1),
            L1PortalEvents::from_deposits(vec![d1.clone()]),
            vec![],
        );
        let hash_after_d1 = queue.enqueued_head_hash();
        assert_ne!(hash_after_d1, B256::ZERO);

        // Verify hash is deterministic
        let mut queue2 = PendingDeposits::default();
        queue2.enqueue(
            make_test_header(1),
            L1PortalEvents::from_deposits(vec![d1]),
            vec![],
        );
        assert_eq!(hash_after_d1, queue2.enqueued_head_hash);

        let d2 = L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 2000,
            fee: 0,
            memo: B256::ZERO,
        });

        queue.enqueue(
            make_test_header(2),
            L1PortalEvents::from_deposits(vec![d2]),
            vec![],
        );
        let hash_after_d2 = queue.enqueued_head_hash();
        assert_ne!(hash_after_d2, hash_after_d1);
    }

    #[test]
    fn test_process_deposits_transition() {
        let deposits = vec![
            L1Deposit::Regular(Deposit {
                token: address!("0x0000000000000000000000000000000000001000"),
                sender: address!("0x0000000000000000000000000000000000000001"),
                to: address!("0x0000000000000000000000000000000000000002"),
                amount: 1000,
                fee: 0,
                memo: B256::ZERO,
            }),
            L1Deposit::Regular(Deposit {
                token: address!("0x0000000000000000000000000000000000001000"),
                sender: address!("0x0000000000000000000000000000000000000003"),
                to: address!("0x0000000000000000000000000000000000000004"),
                amount: 2000,
                fee: 0,
                memo: B256::ZERO,
            }),
        ];

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(transition.prev_processed_hash, B256::ZERO);
        assert_ne!(transition.next_processed_hash, B256::ZERO);

        // Second batch with no deposits should be a no-op
        let transition2 = PendingDeposits::transition(transition.next_processed_hash, &[]);
        assert_eq!(
            transition2.prev_processed_hash,
            transition.next_processed_hash
        );
        assert_eq!(
            transition2.next_processed_hash,
            transition.next_processed_hash
        );
    }

    #[test]
    fn test_queue_and_process_deposits_hashes_match() {
        let mut queue = PendingDeposits::default();

        let deposits = vec![L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 500,
            fee: 0,
            memo: FixedBytes::from([0xABu8; 32]),
        })];

        queue.enqueue(
            make_test_header(1),
            L1PortalEvents::from_deposits(deposits.clone()),
            vec![],
        );

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(queue.enqueued_head_hash(), transition.next_processed_hash);
    }

    #[test]
    fn test_drain_returns_block_grouped_deposits() {
        let mut queue = PendingDeposits::default();

        let d1 = L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 100,
            fee: 0,
            memo: B256::ZERO,
        });

        let d2 = L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 200,
            fee: 0,
            memo: B256::ZERO,
        });

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![d1]), vec![]);
        queue.enqueue(
            make_chained_header(11, h10_hash),
            L1PortalEvents::from_deposits(vec![d2]),
            vec![],
        );

        let blocks = queue.drain();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].header.number(), 10);
        assert_eq!(blocks[0].events.deposits.len(), 1);
        assert_eq!(blocks[1].header.number(), 11);
        assert_eq!(blocks[1].events.deposits.len(), 1);

        // After drain, pending is empty
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn test_encrypted_deposit_hash_chain() {
        let token = address!("0x0000000000000000000000000000000000001000");
        let sender = address!("0x0000000000000000000000000000000000001234");

        let encrypted = EncryptedDeposit {
            token,
            sender,
            amount: 1_000_000,
            fee: 0,
            key_index: U256::ZERO,
            ephemeral_pubkey_x: B256::with_last_byte(0xAA),
            ephemeral_pubkey_y_parity: 0x02,
            ciphertext: vec![0x42u8; 64],
            nonce: [0x01; 12],
            tag: [0x02; 16],
        };

        // Compute via PendingDeposits (Rust implementation)
        let transition =
            PendingDeposits::transition(B256::ZERO, &[L1Deposit::Encrypted(encrypted.clone())]);

        // Compute expected hash via direct Solidity-compatible encoding
        let abi_encrypted = abi::EncryptedDeposit {
            token: encrypted.token,
            sender: encrypted.sender,
            amount: encrypted.amount,
            keyIndex: encrypted.key_index,
            encrypted: abi::EncryptedDepositPayload {
                ephemeralPubkeyX: encrypted.ephemeral_pubkey_x,
                ephemeralPubkeyYParity: encrypted.ephemeral_pubkey_y_parity,
                ciphertext: encrypted.ciphertext.clone().into(),
                nonce: encrypted.nonce.into(),
                tag: encrypted.tag.into(),
            },
        };
        let expected = keccak256((DepositType::Encrypted, abi_encrypted, B256::ZERO).abi_encode());

        assert_eq!(
            transition.next_processed_hash, expected,
            "encrypted deposit hash chain must match Solidity keccak256(abi.encode(Encrypted, deposit, prevHash))"
        );
        assert_ne!(
            transition.next_processed_hash,
            B256::ZERO,
            "hash should be non-zero"
        );
    }

    #[test]
    fn test_mixed_deposit_hash_chain() {
        let token = address!("0x0000000000000000000000000000000000001000");
        let sender = address!("0x0000000000000000000000000000000000001111");
        let recipient = address!("0x000000000000000000000000000000000000A11C");

        let regular = Deposit {
            token,
            sender,
            to: recipient,
            amount: 500_000,
            fee: 0,
            memo: B256::ZERO,
        };

        let encrypted = EncryptedDeposit {
            token,
            sender,
            amount: 300_000,
            fee: 0,
            key_index: U256::from(1u64),
            ephemeral_pubkey_x: B256::with_last_byte(0xBB),
            ephemeral_pubkey_y_parity: 0x03,
            ciphertext: vec![0x55u8; 64],
            nonce: [0x0A; 12],
            tag: [0x0B; 16],
        };

        let deposits = vec![
            L1Deposit::Regular(regular.clone()),
            L1Deposit::Encrypted(encrypted.clone()),
        ];

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        // Manually compute expected chain
        let hash_1 = keccak256(
            (
                DepositType::Regular,
                abi::Deposit {
                    token: regular.token,
                    sender: regular.sender,
                    to: regular.to,
                    amount: regular.amount,
                    memo: regular.memo,
                },
                B256::ZERO,
            )
                .abi_encode(),
        );

        let hash_2 = keccak256(
            (
                DepositType::Encrypted,
                abi::EncryptedDeposit {
                    token: encrypted.token,
                    sender: encrypted.sender,
                    amount: encrypted.amount,
                    keyIndex: encrypted.key_index,
                    encrypted: abi::EncryptedDepositPayload {
                        ephemeralPubkeyX: encrypted.ephemeral_pubkey_x,
                        ephemeralPubkeyYParity: encrypted.ephemeral_pubkey_y_parity,
                        ciphertext: encrypted.ciphertext.into(),
                        nonce: encrypted.nonce.into(),
                        tag: encrypted.tag.into(),
                    },
                },
                hash_1,
            )
                .abi_encode(),
        );

        assert_eq!(transition.prev_processed_hash, B256::ZERO);
        assert_eq!(transition.next_processed_hash, hash_2);
    }

    #[test]
    fn test_enqueue_and_transition_consistency() {
        let token = address!("0x0000000000000000000000000000000000001000");
        let sender = address!("0x0000000000000000000000000000000000001234");

        let encrypted = EncryptedDeposit {
            token,
            sender,
            amount: 750_000,
            fee: 0,
            key_index: U256::from(2u64),
            ephemeral_pubkey_x: B256::with_last_byte(0xCC),
            ephemeral_pubkey_y_parity: 0x02,
            ciphertext: vec![0x99u8; 64],
            nonce: [0x03; 12],
            tag: [0x04; 16],
        };

        let deposits = vec![L1Deposit::Encrypted(encrypted)];

        // Path 1: enqueue into PendingDeposits
        let mut pending = PendingDeposits::default();
        let header = make_test_header(1);
        pending.enqueue(
            header,
            L1PortalEvents::from_deposits(deposits.clone()),
            vec![],
        );

        // Path 2: compute transition directly
        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(
            pending.enqueued_head_hash, transition.next_processed_hash,
            "enqueue and transition must produce the same hash"
        );
    }

    #[test]
    fn test_last_enqueued_survives_pop_and_drain() {
        let queue = DepositQueue::new();

        // Initially empty
        assert!(queue.last_enqueued().is_none());

        let h100 = make_test_header(100);
        let h100_hash = header_hash(&h100);
        queue.enqueue(h100, L1PortalEvents::from_deposits(vec![]), vec![]);
        let h101 = make_chained_header(101, h100_hash);
        let h101_hash = header_hash(&h101);
        queue.enqueue(h101, L1PortalEvents::from_deposits(vec![]), vec![]);
        let h102 = make_chained_header(102, h101_hash);
        queue.enqueue(h102, L1PortalEvents::from_deposits(vec![]), vec![]);

        let last = queue.last_enqueued().unwrap();
        assert_eq!(last.number, 102);

        // Confirm all blocks — last_enqueued must still report 102
        assert!(queue.peek().is_some());
        confirm_shared(&queue);
        assert!(queue.peek().is_some());
        confirm_shared(&queue);
        assert!(queue.peek().is_some());
        confirm_shared(&queue);
        assert!(queue.peek().is_none());

        let last = queue.last_enqueued().unwrap();
        assert_eq!(last.number, 102, "last_enqueued must survive confirm");

        // Enqueue more (continuing from 102), then drain — last_enqueued must still track
        let h102_hash = last.hash;
        let h103 = make_chained_header(103, h102_hash);
        let h103_hash = header_hash(&h103);
        queue.enqueue(h103, L1PortalEvents::from_deposits(vec![]), vec![]);
        queue.enqueue(
            make_chained_header(104, h103_hash),
            L1PortalEvents::from_deposits(vec![]),
            vec![],
        );
        assert_eq!(queue.last_enqueued().unwrap().number, 104);

        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(
            queue.last_enqueued().unwrap().number,
            104,
            "last_enqueued must survive drain"
        );
    }

    #[test]
    fn test_try_enqueue_sequential_append() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        assert_eq!(queue.pending_len(), 2);
    }

    #[test]
    fn test_try_enqueue_gap_returns_need_backfill() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Skip block 11, try to enqueue 12
        let h3 = make_test_header(12);
        match queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_try_enqueue_duplicate() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(
                seal(h1.clone()),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Duplicate
        ));
    }

    #[test]
    fn test_try_enqueue_reorg_purges_stale() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        let h2_hash = header_hash(&h2);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h3 = make_chained_header(12, h2_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        assert_eq!(queue.pending_len(), 3);

        // Reorg at height 11 — use a different header (different gas_limit makes the hash different)
        let mut h2_reorg = make_chained_header(11, h1_hash);
        h2_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // Blocks 12 and the old 11 should be purged, replaced by new 11
        assert_eq!(queue.pending_len(), 2);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 10);
        assert_eq!(queue.pending_block(1).unwrap().header.number(), 11);
    }

    #[test]
    fn test_try_enqueue_parent_mismatch_at_tip() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Block 12 with wrong parent hash — purges block 11, needs backfill
        let h3 = make_chained_header(12, B256::with_last_byte(0xDE));
        match queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_purge_rolls_back_deposit_hash() {
        let mut queue = PendingDeposits::default();
        let token = address!("0x0000000000000000000000000000000000001000");

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        let d1 = L1Deposit::Regular(Deposit {
            token,
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 100,
            fee: 0,
            memo: B256::ZERO,
        });
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![d1]), vec![]),
            EnqueueOutcome::Accepted
        ));
        let hash_after_h1 = queue.enqueued_head_hash();

        let h2 = make_chained_header(11, h1_hash);
        let d2 = L1Deposit::Regular(Deposit {
            token,
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 200,
            fee: 0,
            memo: B256::ZERO,
        });
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![d2]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Hash advanced past h1
        assert_ne!(queue.enqueued_head_hash(), hash_after_h1);

        // Now reorg at height 11 — different header
        let mut h2_reorg = make_chained_header(11, h1_hash);
        h2_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // Hash should have rolled back to after h1 (since h2_reorg has no deposits)
        assert_eq!(queue.enqueued_head_hash(), hash_after_h1);
    }

    fn make_deposit(amount: u128) -> L1Deposit {
        L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount,
            fee: 0,
            memo: B256::ZERO,
        })
    }

    #[test]
    fn test_pop_advances_processed_head_hash() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(
            h2,
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );

        let h3 = make_chained_header(3, h2_hash);
        queue.enqueue(
            h3,
            L1PortalEvents::from_deposits(vec![make_deposit(300)]),
            vec![],
        );

        let hash_after_all = queue.enqueued_head_hash();

        // Confirm block 1
        let peeked = queue.peek().unwrap().clone();
        assert_eq!(peeked.header.number(), 1);
        confirm(&mut queue);
        assert_eq!(queue.processed_head_hash, peeked.queue_hash_after);

        // queue.enqueued_head_hash() hasn't changed
        assert_eq!(queue.enqueued_head_hash(), hash_after_all);

        // Recompute expected hash from processed_head_hash + remaining deposits (blocks 2, 3)
        let remaining_deposits: Vec<L1Deposit> = queue
            .pending_blocks()
            .iter()
            .flat_map(|b| b.events.deposits.clone())
            .collect();
        let transition =
            PendingDeposits::transition(queue.processed_head_hash, &remaining_deposits);
        assert_eq!(transition.next_processed_hash, queue.enqueued_head_hash());
    }

    #[test]
    fn test_purge_after_pops() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(h2, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h3 = make_chained_header(3, h2_hash);
        let h3_hash = header_hash(&h3);
        queue.enqueue(h3, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h4 = make_chained_header(4, h3_hash);
        let h4_hash = header_hash(&h4);
        queue.enqueue(h4, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h5 = make_chained_header(5, h4_hash);
        queue.enqueue(h5, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Pop blocks 1 and 2
        confirm(&mut queue);
        confirm(&mut queue);
        assert_eq!(queue.pending_len(), 3); // blocks 3, 4, 5

        let hash_after_block3 = queue.pending_block(0).unwrap().queue_hash_after;

        // Trigger purge at block 4: different header at height 4
        let mut h4_reorg = make_chained_header(4, h3_hash);
        h4_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(
                seal(h4_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // Pending should have blocks 3 and new-4
        assert_eq!(queue.pending_len(), 2);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 3);
        assert_eq!(queue.pending_block(1).unwrap().header.number(), 4);

        // New block 4 has no deposits, so hash == hash after block 3's deposits
        assert_eq!(queue.enqueued_head_hash(), hash_after_block3);
    }

    #[test]
    fn test_purge_first_pending_after_pop() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(
            h2,
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );

        let h3 = make_chained_header(3, h2_hash);
        queue.enqueue(
            h3,
            L1PortalEvents::from_deposits(vec![make_deposit(300)]),
            vec![],
        );

        // Pop block 1 — processed_head_hash advances
        let popped = confirm(&mut queue);
        let base_after_pop = popped.queue_hash_after;
        assert_eq!(queue.processed_head_hash, base_after_pop);
        assert_eq!(queue.pending_len(), 2); // blocks 2, 3

        // Purge from block 2 by enqueueing a different block 2
        let mut h2_reorg = make_chained_header(2, B256::with_last_byte(0xFF));
        h2_reorg.inner.gas_limit = 777;
        // This block has a different hash at height 2, so purge_from(0) fires.
        // Queue becomes empty, then the new block is accepted as anchor.
        let outcome = queue.try_enqueue(
            seal(h2_reorg),
            L1PortalEvents::from_deposits(vec![]),
            vec![],
        );
        assert!(matches!(outcome, EnqueueOutcome::Accepted));

        // After purge and re-anchor, pending has just the new block 2
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 2);

        // processed_head_hash should still be what it was after popping block 1
        assert_eq!(queue.processed_head_hash, base_after_pop);
    }

    #[test]
    fn test_backfill_then_duplicate_redelivery() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        // Try to enqueue block 3 (skipping 2) => NeedBackfill
        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        let h3 = make_chained_header(3, h2_hash);
        let h3_sealed = seal(h3);
        match queue.try_enqueue(
            seal(make_test_header(3)),
            L1PortalEvents::from_deposits(vec![]),
            vec![],
        ) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 2);
                assert_eq!(to, 2);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }

        // Backfill: enqueue block 2, then block 3
        queue.enqueue(
            h2,
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );
        assert!(matches!(
            queue.try_enqueue(
                h3_sealed.clone(),
                L1PortalEvents::from_deposits(vec![make_deposit(300)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        let hash_before = queue.enqueued_head_hash();
        let len_before = queue.pending_len();

        // Re-deliver block 3 (same sealed header) => Duplicate
        assert!(matches!(
            queue.try_enqueue(
                h3_sealed,
                L1PortalEvents::from_deposits(vec![make_deposit(300)]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));

        assert_eq!(queue.enqueued_head_hash(), hash_before);
        assert_eq!(queue.pending_len(), len_before);
    }

    #[test]
    fn test_zero_deposit_block_hash_invariant() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(h2, L1PortalEvents::from_deposits(vec![]), vec![]); // no deposits

        let h3 = make_chained_header(3, h2_hash);
        let d3 = make_deposit(300);
        queue.enqueue(h3, L1PortalEvents::from_deposits(vec![d3.clone()]), vec![]);

        // Block 2 has no deposits => queue_hash_before == queue_hash_after
        assert_eq!(
            queue.pending_block(1).unwrap().queue_hash_before,
            queue.pending_block(1).unwrap().queue_hash_after,
            "zero-deposit block must not change queue hash"
        );

        let hash_after_all_original = queue.enqueued_head_hash();

        // Purge at block 2 (different header) — purges blocks 2, 3
        let mut h2_reorg = make_chained_header(2, h1_hash);
        h2_reorg.inner.gas_limit = 888;
        let h2_reorg_hash = header_hash(&h2_reorg);
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // After purge, only block 1 and new block 2 remain
        assert_eq!(queue.pending_len(), 2);
        let hash_after_block1 = queue.pending_block(0).unwrap().queue_hash_after;
        // New block 2 has no deposits so hash == hash after block 1
        assert_eq!(queue.enqueued_head_hash(), hash_after_block1);

        // Re-enqueue new block 3 with same deposits as original
        let h3_new = make_chained_header(3, h2_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(
                seal(h3_new),
                L1PortalEvents::from_deposits(vec![d3]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // The hash should match original because the deposit content and
        // chain of hashes are identical (both block 2 variants had no deposits)
        assert_eq!(
            queue.enqueued_head_hash(),
            hash_after_all_original,
            "hash should be identical when deposit content is the same"
        );
    }

    // --- Disconnected scenario tests (parent mismatch on drained queue) ---

    #[test]
    fn test_disconnected_after_full_drain() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain everything
        confirm(&mut queue);
        confirm(&mut queue);
        assert!(queue.pending_len() == 0);
        assert_eq!(queue.last_enqueued().unwrap().number, 11);

        // Block 12 arrives with wrong parent — consumed block 11 was reorged
        let h3_bad = make_chained_header(12, B256::with_last_byte(0xDE));
        match queue.try_enqueue(seal(h3_bad), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                // Must re-fetch from block 11 (the consumed block that was reorged)
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }

        // last_enqueued must be cleared so backfill can re-enqueue block 11
        assert!(
            queue.last_enqueued().is_none(),
            "last_enqueued must be cleared after parent mismatch on drained queue"
        );

        // Backfill tries to re-enqueue the reorged block 11 — must be Duplicate
        // because last_processed knows block 11 was already consumed by the engine.
        let h2_reorg = make_test_header(11);
        let h2_reorg_hash = header_hash(&h2_reorg);
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Duplicate
        ));

        // Block 12 on the new chain is accepted as the immediate successor
        // of the consumed window (parent hash mismatch is expected here —
        // the builder will detect it).
        let h3 = make_chained_header(12, h2_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 12);
    }

    #[test]
    fn test_disconnected_after_partial_drain() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(
                seal(h1),
                L1PortalEvents::from_deposits(vec![make_deposit(100)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        let h2_hash = header_hash(&h2);
        assert!(matches!(
            queue.try_enqueue(
                seal(h2),
                L1PortalEvents::from_deposits(vec![make_deposit(200)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        let h3 = make_chained_header(12, h2_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Pop only block 10
        confirm(&mut queue);
        assert_eq!(queue.pending_len(), 2); // blocks 11, 12

        // Block 13 with wrong parent — this is a normal parent mismatch on the
        // non-empty queue path, should purge block 12 and request backfill
        let h4_bad = make_chained_header(13, B256::with_last_byte(0xAB));
        match queue.try_enqueue(seal(h4_bad), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 12);
                assert_eq!(to, 12);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_disconnected_recovery_accepts_correct_block() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain
        confirm(&mut queue);
        assert!(queue.pending_len() == 0);

        // Wrong parent → NeedBackfill
        let h2_bad = make_chained_header(11, B256::with_last_byte(0xFF));
        assert!(matches!(
            queue.try_enqueue(seal(h2_bad), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::NeedBackfill { .. }
        ));

        // Correct parent → Accepted
        let h2_good = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_good),
                L1PortalEvents::from_deposits(vec![make_deposit(500)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 11);
    }

    #[test]
    fn test_disconnected_with_multi_block_gap() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain
        confirm(&mut queue);

        // Block 14 arrives — gap of 11..13 plus wrong parent is moot because
        // the gap check triggers first
        let h5 = make_test_header(14);
        match queue.try_enqueue(seal(h5), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11);
                assert_eq!(to, 13);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_duplicate_on_drained_queue() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain everything
        confirm(&mut queue);
        confirm(&mut queue);
        assert!(queue.pending_len() == 0);

        // Re-deliver block 10 or 11 — should be Duplicate
        assert!(matches!(
            queue.try_enqueue(
                seal(make_test_header(10)),
                L1PortalEvents::from_deposits(vec![]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));
        assert!(matches!(
            queue.try_enqueue(
                seal(make_chained_header(11, h1_hash)),
                L1PortalEvents::from_deposits(vec![]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));
    }

    #[test]
    fn test_disconnected_preserves_processed_head_hash_and_deposits() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(
                seal(h1),
                L1PortalEvents::from_deposits(vec![make_deposit(100)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        // Pop and record processed_head_hash
        let popped = confirm(&mut queue);
        let base = queue.processed_head_hash;
        assert_eq!(base, popped.queue_hash_after);

        // Disconnected block should not alter processed_head_hash
        let h2_bad = make_chained_header(11, B256::with_last_byte(0xBB));
        assert!(matches!(
            queue.try_enqueue(seal(h2_bad), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::NeedBackfill { .. }
        ));
        assert_eq!(
            queue.processed_head_hash, base,
            "processed_head_hash must not change on NeedBackfill"
        );
        assert_eq!(
            queue.enqueued_head_hash(),
            base,
            "enqueued_head_hash must not change on NeedBackfill"
        );
        assert!(queue.pending_len() == 0);
    }

    #[test]
    fn test_reconnect_duplicate_does_not_clear_last_enqueued() {
        // A reconnect may re-deliver the same block we already consumed.
        // This must return Duplicate without clearing last_enqueued.
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        queue.enqueue(h1.clone(), L1PortalEvents::from_deposits(vec![]), vec![]);

        // Drain
        confirm(&mut queue);
        assert!(queue.pending_len() == 0);
        assert_eq!(queue.last_enqueued().unwrap().number, 10);

        // Re-deliver same block 10 — must be Duplicate, last_enqueued preserved
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(
            queue.last_enqueued().unwrap().number,
            10,
            "last_enqueued must not be cleared on Duplicate"
        );
    }

    #[test]
    fn test_backfill_overlap_idempotency() {
        // If backfill re-delivers blocks already in pending, duplicates are
        // tolerated and the queue state is unchanged.
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1.clone(),
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(11, h1_hash);
        queue.enqueue(
            h2.clone(),
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );

        let hash_before = queue.enqueued_head_hash();
        let len_before = queue.pending_len();

        // Re-enqueue both — should be Duplicate, no state change
        assert!(matches!(
            queue.try_enqueue(
                seal(h1),
                L1PortalEvents::from_deposits(vec![make_deposit(100)]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));
        assert!(matches!(
            queue.try_enqueue(
                seal(h2),
                L1PortalEvents::from_deposits(vec![make_deposit(200)]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(queue.enqueued_head_hash(), hash_before);
        assert_eq!(queue.pending_len(), len_before);
    }

    #[test]
    fn test_reorg_within_pending_recomputes_hash() {
        // Reorg at a middle block in pending should purge from that point,
        // accept the new block, and recompute the hash chain consistently.
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(
            h10,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );
        let hash_after_10 = queue.enqueued_head_hash();

        let h11 = make_chained_header(11, h10_hash);
        let h11_hash = header_hash(&h11);
        queue.enqueue(
            h11,
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );

        let h12 = make_chained_header(12, h11_hash);
        let h12_hash = header_hash(&h12);
        queue.enqueue(
            h12,
            L1PortalEvents::from_deposits(vec![make_deposit(300)]),
            vec![],
        );

        let h13 = make_chained_header(13, h12_hash);
        queue.enqueue(
            h13,
            L1PortalEvents::from_deposits(vec![make_deposit(400)]),
            vec![],
        );

        assert_eq!(queue.pending_len(), 4);

        // Reorg at block 11 — new header with same parent but different content
        let mut h11_reorg = make_chained_header(11, h10_hash);
        h11_reorg.inner.gas_limit = 42;
        let h11_reorg_hash = header_hash(&h11_reorg);

        assert!(matches!(
            queue.try_enqueue(
                seal(h11_reorg),
                L1PortalEvents::from_deposits(vec![make_deposit(999)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        // Blocks 12, 13 purged; now have 10 + new 11
        assert_eq!(queue.pending_len(), 2);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 10);
        assert_eq!(queue.pending_block(1).unwrap().header.number(), 11);
        assert_eq!(queue.last_enqueued().unwrap().number, 11);

        // Hash should differ from original because deposit content changed
        assert_ne!(queue.enqueued_head_hash(), hash_after_10);

        // Can continue building on the new fork
        let h12_new = make_chained_header(12, h11_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h12_new), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));
        assert_eq!(queue.pending_len(), 3);
    }

    #[test]
    fn test_drained_reorg_same_height_returns_duplicate() {
        // If the queue is drained and we receive the same block number with
        // the same hash (not a reorg), it must be Duplicate.
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h11 = make_chained_header(11, h10_hash);
        queue.enqueue(h11.clone(), L1PortalEvents::from_deposits(vec![]), vec![]);

        // Drain
        confirm(&mut queue);
        confirm(&mut queue);

        // Re-deliver block 11 with same hash — Duplicate, last_enqueued intact
        assert!(matches!(
            queue.try_enqueue(seal(h11), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(queue.last_enqueued().unwrap().number, 11);
    }

    // --- last_processed floor tests ---

    #[test]
    fn test_pop_sets_last_processed() {
        let mut queue = PendingDeposits::default();

        assert!(queue.last_processed().is_none());

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h2 = make_chained_header(11, h1_hash);
        queue.enqueue(h2, L1PortalEvents::from_deposits(vec![]), vec![]);

        let popped = confirm(&mut queue);
        assert_eq!(queue.last_processed().unwrap().number, 10);
        assert_eq!(queue.last_processed().unwrap().hash, popped.header.hash());

        confirm(&mut queue);
        assert_eq!(queue.last_processed().unwrap().number, 11);
    }

    #[test]
    fn test_drain_sets_last_processed() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h2 = make_chained_header(11, h1_hash);
        queue.enqueue(h2, L1PortalEvents::from_deposits(vec![]), vec![]);

        let drained = queue.drain();
        assert_eq!(queue.last_processed().unwrap().number, 11);
        assert_eq!(
            queue.last_processed().unwrap().hash,
            drained.last().unwrap().header.hash()
        );
    }

    #[test]
    fn test_reorg_of_consumed_block_skips_stale_during_backfill() {
        // Simulates the exact production bug:
        // 1. Blocks 10, 11 enqueued and consumed (popped)
        // 2. L1 reorgs at block 11 — new block 12 arrives with wrong parent
        // 3. try_enqueue clears last_enqueued, returns NeedBackfill{11, 11}
        // 4. Backfill re-enqueues NEW block 11 — must be Duplicate (already consumed)
        // 5. Backfill enqueues NEW block 12 — must be Accepted
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h11 = make_chained_header(11, h10_hash);
        let _h11_hash = header_hash(&h11);
        queue.enqueue(h11, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Engine consumes both blocks
        confirm(&mut queue); // block 10
        confirm(&mut queue); // block 11
        assert!(queue.pending_len() == 0);
        assert_eq!(queue.last_processed().unwrap().number, 11);
        assert_eq!(queue.last_enqueued().unwrap().number, 11);

        // New block 12 arrives with parent pointing to NEW (reorged) block 11
        let h12_new = make_chained_header(12, B256::with_last_byte(0xDE));
        match queue.try_enqueue(seal(h12_new), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
        // last_enqueued cleared by the parent mismatch path
        assert!(queue.last_enqueued().is_none());

        // Backfill: try to re-enqueue NEW block 11 — must be Duplicate
        // because last_processed knows block 11 was already consumed
        let mut h11_reorg = make_test_header(11);
        h11_reorg.inner.gas_limit = 999;
        let h11_reorg_hash = header_hash(&h11_reorg);
        assert!(matches!(
            queue.try_enqueue(
                seal(h11_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Duplicate
        ));

        // Backfill: enqueue NEW block 12 — must be Accepted
        // (immediate successor of consumed block, parent hash mismatch is
        // expected and will be caught by the builder)
        let h12 = make_chained_header(12, h11_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h12), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Queue now has exactly one block: NEW block 12
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 12);
    }

    #[test]
    fn test_consumed_reorg_gap_uses_last_processed_floor() {
        // After consuming blocks and clearing last_enqueued, a block arriving
        // with a gap should use last_processed as the floor.
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let _h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![]), vec![]);
        confirm(&mut queue);

        // Simulate last_enqueued being cleared (reorg path)
        queue.clear_last_enqueued();

        // Block 13 arrives — gap from 11..12
        let h13 = make_test_header(13);
        match queue.try_enqueue(seal(h13), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11); // last_processed.number + 1
                assert_eq!(to, 12);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_builder_skips_stale_blocks_in_queue() {
        // Simulates the builder's perspective: queue contains a stale block
        // (number < expected) followed by the correct block. The builder
        // should skip the stale one and use the correct one.
        let queue = DepositQueue::new();

        // Enqueue block 10 (stale — zone already processed it)
        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Enqueue block 11 (the one the builder actually needs)
        let h11 = make_chained_header(11, h10_hash);
        queue.enqueue(h11, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Builder expects block 11 (tempoBlockNumber=10, expected=11).
        // Peek/confirm loop should skip block 10 and use the correct one.
        let expected = 11u64;
        let l1_block = loop {
            let block = queue.peek().expect("queue should not be empty");
            if block.header.number() < expected {
                confirm_shared(&queue);
                continue;
            }
            break block;
        };
        assert_eq!(l1_block.header.number(), 11);
    }

    #[test]
    fn test_reorg_consumed_then_continue_on_new_chain() {
        // Full end-to-end scenario: reorg of consumed block, backfill skips it,
        // zone gets the correct next block. Subsequent blocks also work.
        let mut queue = PendingDeposits::default();

        // Build a 3-block chain: 100, 101, 102
        let h100 = make_test_header(100);
        let h100_hash = header_hash(&h100);
        queue.enqueue(h100, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h101 = make_chained_header(101, h100_hash);
        let h101_hash = header_hash(&h101);
        queue.enqueue(h101, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h102 = make_chained_header(102, h101_hash);
        queue.enqueue(h102, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Engine consumes all 3
        confirm(&mut queue);
        confirm(&mut queue);
        confirm(&mut queue);
        assert_eq!(queue.last_processed().unwrap().number, 102);

        // L1 reorgs at 102: new block 103 has different parent
        let new_parent = B256::with_last_byte(0xAA);
        let h103 = make_chained_header(103, new_parent);
        match queue.try_enqueue(seal(h103), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 102);
                assert_eq!(to, 102);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }

        // Backfill: re-enqueue NEW block 102 → Duplicate (consumed)
        let mut h102_new = make_test_header(102);
        h102_new.inner.gas_limit = 42;
        let h102_new_hash = header_hash(&h102_new);
        assert!(matches!(
            queue.try_enqueue(
                seal(h102_new),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Duplicate
        ));

        // Backfill: enqueue NEW block 103 → Accepted
        let h103 = make_chained_header(103, h102_new_hash);
        let h103_hash = header_hash(&h103);
        assert!(matches!(
            queue.try_enqueue(seal(h103), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Block 104 continues on the new chain
        let h104 = make_chained_header(104, h103_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h104), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        assert_eq!(queue.pending_len(), 2);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 103);
        assert_eq!(queue.pending_block(1).unwrap().header.number(), 104);
    }
}
