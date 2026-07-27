use super::*;
use crate::{abi::DepositType, subscriber::DepositSink};
use alloy_consensus::{Header, ReceiptWithBloom};
use alloy_primitives::{Bloom, Bytes, address};
use alloy_rpc_types_eth::{Header as RpcHeader, TransactionReceipt};
use alloy_sol_types::SolEvent;
use alloy_transport::mock::Asserter;
use serde::Deserialize;
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};
use tempo_alloy::rpc::{TempoHeaderResponse, TempoTransactionReceipt};
use tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS;
use tempo_primitives::{TempoReceipt, TempoTxType};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EncryptedDepositHashFixture {
    previous_hash: String,
    expected_hash: String,
    single_value_tuple_hash: String,
    deposit: EncryptedDepositFixture,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EncryptedDepositFixture {
    token: String,
    sender: String,
    amount: u128,
    tempo_refund_recipient: String,
    key_index: u64,
    encrypted: EncryptedDepositPayloadFixture,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EncryptedDepositPayloadFixture {
    ephemeral_pubkey_x: String,
    ephemeral_pubkey_y_parity: u8,
    ciphertext: String,
    nonce: String,
    tag: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MalformedTempoHeadersFixture {
    trailing_bytes_after_outer_list: String,
    outer_list_length_mismatch: String,
    outer_list_long_length_leading_zero: String,
    difficulty_non_canonical_short_string: String,
    block_number_leading_zero: String,
    extra_data_long_length_below_short_threshold: String,
}

fn encrypted_deposit_hash_fixture() -> EncryptedDepositHashFixture {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../specs/ref-impls/test/fixtures/encryptedDepositHashChain.json"
    )))
    .expect("encrypted deposit hash fixture JSON should decode")
}

fn malformed_tempo_headers_fixture() -> MalformedTempoHeadersFixture {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../specs/ref-impls/test/fixtures/malformedTempoHeaders.json"
    )))
    .expect("malformed Tempo headers fixture JSON should decode")
}

fn parse_fixture_address(value: &str) -> Address {
    value
        .parse()
        .unwrap_or_else(|err| panic!("invalid fixture address {value}: {err}"))
}

fn parse_fixture_b256(value: &str) -> B256 {
    value
        .parse()
        .unwrap_or_else(|err| panic!("invalid fixture bytes32 {value}: {err}"))
}

fn parse_fixture_hex(value: &str) -> Vec<u8> {
    const_hex::decode(value.strip_prefix("0x").unwrap_or(value))
        .unwrap_or_else(|err| panic!("invalid fixture hex {value}: {err}"))
}

fn parse_fixture_fixed<const N: usize>(value: &str, name: &str) -> [u8; N] {
    let bytes = parse_fixture_hex(value);
    assert_eq!(bytes.len(), N, "fixture field {name} should be {N} bytes");
    let mut out = [0; N];
    out.copy_from_slice(&bytes);
    out
}

impl EncryptedDepositFixture {
    fn to_l1_deposit(&self) -> EncryptedDeposit {
        EncryptedDeposit {
            token: parse_fixture_address(&self.token),
            sender: parse_fixture_address(&self.sender),
            amount: self.amount,
            fee: 0,
            tempo_refund_recipient: parse_fixture_address(&self.tempo_refund_recipient),
            key_index: U256::from(self.key_index),
            ephemeral_pubkey_x: parse_fixture_b256(&self.encrypted.ephemeral_pubkey_x),
            ephemeral_pubkey_y_parity: self.encrypted.ephemeral_pubkey_y_parity,
            ciphertext: parse_fixture_hex(&self.encrypted.ciphertext),
            nonce: parse_fixture_fixed(&self.encrypted.nonce, "encrypted.nonce"),
            tag: parse_fixture_fixed(&self.encrypted.tag, "encrypted.tag"),
        }
    }
}

fn make_deposit(amount: u128) -> L1Deposit {
    L1Deposit::Regular(Deposit {
        token: address!("0x0000000000000000000000000000000000001000"),
        sender: address!("0x0000000000000000000000000000000000000001"),
        to: address!("0x0000000000000000000000000000000000000002"),
        amount,
        fee: 0,
        tempo_refund_recipient: address!("0x0000000000000000000000000000000000000001"),
        memo: B256::ZERO,
    })
}

struct SequenceLocalTempoCheckpointReader {
    values: Mutex<VecDeque<u64>>,
    last_value: u64,
}

impl SequenceLocalTempoCheckpointReader {
    fn new(values: impl Into<VecDeque<u64>>) -> Self {
        let values = values.into();
        let last_value = values.back().copied().unwrap_or_default();
        Self {
            values: Mutex::new(values),
            last_value,
        }
    }
}

impl LocalTempoCheckpointReader for SequenceLocalTempoCheckpointReader {
    fn latest_tempo_block_number(&self) -> eyre::Result<u64> {
        let mut values = self.values.lock();
        Ok(values.pop_front().unwrap_or(self.last_value))
    }
}

fn test_subscriber(
    local_state: Arc<dyn LocalTempoCheckpointReader>,
    genesis_tempo_block_number: Option<u64>,
) -> L1Subscriber {
    let portal_address = address!("0x0000000000000000000000000000000000000ABC");

    L1Subscriber {
        config: L1SubscriberConfig {
            l1_rpc_url: "http://127.0.0.1:8545".to_owned(),
            portal_address,
            genesis_tempo_block_number,
            enabled_tokens: crate::state::EnabledTokenRegistry::default(),
            l1_state_cache: crate::L1StateCache::new(),
            block_tracker: L1BlockTracker::default(),
            retain_observations: true,
            l1_fetch_concurrency: 1,
            retry_connection_interval: Duration::from_secs(1),
        },
        local_state,
        deposit_sink: DepositSink::Queue(DepositQueue::default()),
        subscriber_metrics: Default::default(),
    }
}

fn test_observer(
    local_state: Arc<dyn LocalTempoCheckpointReader>,
    genesis_tempo_block_number: Option<u64>,
) -> L1Subscriber {
    let mut subscriber = test_subscriber(local_state, genesis_tempo_block_number);
    subscriber.config.retain_observations = false;
    subscriber.deposit_sink = DepositSink::Observer;
    subscriber
}

#[tokio::test]
async fn l1_block_tracker_waits_for_exact_observation() {
    let tracker = L1BlockTracker::default();
    let anchor = NumHash::new(10, B256::with_last_byte(0x10));
    let waiting = tracker.clone();
    let waiter = tokio::spawn(async move { waiting.wait_for(anchor).await });

    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    tracker.record(anchor).unwrap();
    waiter.await.unwrap().unwrap();
}

#[tokio::test]
async fn l1_block_tracker_returns_receipt_authenticated_portal_events() {
    let tracker = L1BlockTracker::default();
    let anchor = NumHash::new(10, B256::with_last_byte(0x10));
    let events = L1PortalEvents::from_deposits(vec![make_deposit(100)]);
    tracker
        .record_with_portal_events(anchor, events.clone())
        .unwrap();

    let observed = tracker.wait_for_portal_events(anchor).await.unwrap();
    assert_eq!(observed.deposits.len(), 1);
    assert_eq!(
        observed.deposits[0].to_abi_queued_deposit(),
        events.deposits[0].to_abi_queued_deposit()
    );
}

#[test]
fn observed_portal_events_require_complete_advance_tempo_inputs() {
    let events = L1PortalEvents {
        deposits: vec![make_deposit(100), make_deposit(200)],
        enabled_tokens: vec![EnabledToken {
            token: address!("0x20C0000000000000000000000000000000000001"),
            name: "Alpha USD".to_owned(),
            symbol: "aUSD".to_owned(),
            currency: "USD".to_owned(),
        }],
    };
    let deposits: Vec<_> = events
        .deposits
        .iter()
        .map(L1Deposit::to_abi_queued_deposit)
        .collect();
    let enabled_tokens: Vec<_> = events
        .enabled_tokens
        .iter()
        .map(EnabledToken::to_abi)
        .collect();

    // Rejection is a sequencer decision and does not change the authenticated deposit identity.
    events
        .validate_advance_tempo_inputs(&deposits, &enabled_tokens)
        .unwrap();

    let partial = events
        .validate_advance_tempo_inputs(&deposits[..1], &enabled_tokens)
        .unwrap_err();
    assert!(partial.to_string().contains("deposit count"));

    let mut fabricated = deposits.clone();
    fabricated[1].depositData = Bytes::from_static(b"fabricated");
    let fabricated = events
        .validate_advance_tempo_inputs(&fabricated, &enabled_tokens)
        .unwrap_err();
    assert!(fabricated.to_string().contains("deposit 1"));

    let missing_token = events
        .validate_advance_tempo_inputs(&deposits, &[])
        .unwrap_err();
    assert!(missing_token.to_string().contains("token enables"));
}

#[tokio::test]
async fn l1_block_tracker_rejects_conflicts_and_missing_heights() {
    let tracker = L1BlockTracker::default();
    let block_10 = NumHash::new(10, B256::with_last_byte(0x10));
    let block_11 = NumHash::new(11, B256::with_last_byte(0x11));
    tracker.record(block_10).unwrap();
    tracker.record(block_11).unwrap();
    tracker.record(block_11).unwrap();

    let conflict = NumHash::new(11, B256::with_last_byte(0xff));
    assert!(tracker.wait_for(conflict).await.is_err());
    assert!(
        tracker
            .record(NumHash::new(13, B256::with_last_byte(0x13)))
            .is_err()
    );
}

#[tokio::test]
async fn l1_block_tracker_prunes_only_consumed_observations() {
    let tracker = L1BlockTracker::default();
    for number in 10..=12 {
        tracker
            .record(NumHash::new(number, B256::with_last_byte(number as u8)))
            .unwrap();
    }

    tracker.prune_through(10);
    assert_eq!(tracker.observed_hash(10), None);
    assert_eq!(tracker.observed_hash(11), Some(B256::with_last_byte(11)));
    assert_eq!(tracker.latest().unwrap().number, 12);
    assert!(
        tracker
            .wait_for(NumHash::new(10, B256::with_last_byte(10)))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn l1_block_tracker_backpressures_at_one_hour_lookahead() {
    let tracker = L1BlockTracker::default();
    let consumed = 100;
    tracker.initialize_consumed_through(consumed);

    for number in consumed + 1..=consumed + MAX_FOLLOWER_L1_LOOKAHEAD_BLOCKS {
        tracker
            .record(NumHash::new(number, B256::with_last_byte(number as u8)))
            .unwrap();
    }

    let blocked_number = consumed + MAX_FOLLOWER_L1_LOOKAHEAD_BLOCKS + 1;
    assert!(!tracker.has_capacity_for(blocked_number));
    assert_eq!(tracker.next_observation_number(), Some(blocked_number));
    assert!(
        tracker
            .record(NumHash::new(
                blocked_number,
                B256::with_last_byte(blocked_number as u8),
            ))
            .is_err()
    );

    let waiting = tracker.clone();
    let waiter = tokio::spawn(async move { waiting.wait_for_capacity(blocked_number).await });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());

    tracker.prune_through(consumed + 1);
    waiter.await.unwrap().unwrap();
    assert!(tracker.has_capacity_for(blocked_number));
}

#[test]
fn l1_block_tracker_rejects_first_observation_above_persisted_successor() {
    let tracker = L1BlockTracker::default();
    tracker.initialize_consumed_through(10);

    let skipped = tracker
        .record(NumHash::new(12, B256::with_last_byte(12)))
        .unwrap_err();
    assert!(
        skipped
            .to_string()
            .contains("non-contiguous first L1 observation")
    );
    assert_eq!(tracker.latest(), None);
    assert_eq!(tracker.next_observation_number(), Some(11));

    tracker
        .record(NumHash::new(11, B256::with_last_byte(11)))
        .unwrap();
    assert_eq!(tracker.latest().unwrap().number, 11);
}

#[test]
fn subscriber_applies_state_and_records_observation() {
    let subscriber = test_subscriber(Arc::new(SequenceLocalTempoCheckpointReader::new([9])), None);
    let header = make_test_header(10);
    let sealed = seal(header);
    let anchor = sealed.num_hash();
    let cached_address = address!("0x0000000000000000000000000000000000000ABC");
    let cached_slot = B256::with_last_byte(1);
    let cached_value = B256::with_last_byte(2);

    {
        let mut cache = subscriber.config.l1_state_cache.lock();
        cache.invalidate_and_set_anchor(9, []);
        cache.set(cached_address, cached_slot, 9, cached_value);
    }
    subscriber.update_l1_state_anchor(10, &HashSet::new());
    subscriber.config.block_tracker.record(anchor).unwrap();

    assert_eq!(
        subscriber
            .config
            .l1_state_cache
            .lock()
            .get(cached_address, cached_slot, 10),
        Some(cached_value)
    );
    assert_eq!(
        subscriber.config.block_tracker.observed_hash(10),
        Some(anchor.hash)
    );
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

fn header_response(header: TempoHeader) -> TempoHeaderResponse {
    TempoHeaderResponse {
        inner: RpcHeader {
            hash: header_hash(&header),
            inner: header,
            total_difficulty: None,
            size: None,
        },
        timestamp_millis: 0,
    }
}

fn push_header_and_empty_receipts(asserter: &Asserter, header: TempoHeader) {
    asserter.push_success(&Some(header_response(header)));
    asserter.push_success(&Some(Vec::<TempoTransactionReceipt>::new()));
}

fn make_test_receipt(
    block_number: u64,
    block_hash: B256,
    tx_hash: B256,
    tx_index: u64,
    cumulative_gas_used: u64,
    logs_bloom: Bloom,
) -> TempoTransactionReceipt {
    TempoTransactionReceipt {
        inner: TransactionReceipt {
            inner: ReceiptWithBloom::new(
                TempoReceipt {
                    tx_type: TempoTxType::Legacy,
                    success: true,
                    cumulative_gas_used,
                    logs: vec![],
                },
                logs_bloom,
            ),
            transaction_hash: tx_hash,
            transaction_index: Some(tx_index),
            block_hash: Some(block_hash),
            block_number: Some(block_number),
            gas_used: cumulative_gas_used,
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

fn calculate_test_receipts_root(receipts: &[TempoTransactionReceipt]) -> B256 {
    let receipts = receipts
        .iter()
        .map(|receipt| {
            receipt
                .inner
                .inner
                .clone()
                .map_receipt(|receipt| receipt.map_logs(Into::into))
        })
        .collect::<Vec<_>>();
    alloy_consensus::proofs::calculate_receipt_root(&receipts)
}

#[test]
fn verify_receipts_accepts_matching_root_and_logs_bloom() {
    let block_number = 42;
    let header = make_test_header(block_number);
    let block_hash = header_hash(&header);
    let block = NumHash::new(block_number, block_hash);
    let receipts = vec![
        make_test_receipt(
            block_number,
            block_hash,
            B256::with_last_byte(0x01),
            0,
            21_000,
            Bloom::ZERO,
        ),
        make_test_receipt(
            block_number,
            block_hash,
            B256::with_last_byte(0x02),
            1,
            42_000,
            Bloom::ZERO,
        ),
    ];
    let receipts_root = calculate_test_receipts_root(&receipts);

    verify_receipts(block, receipts_root, Bloom::ZERO, &receipts)
        .expect("matching receipts root should validate");
}

#[test]
fn verify_receipts_rejects_receipts_root_mismatch() {
    let block_number = 42;
    let header = make_test_header(block_number);
    let block_hash = header_hash(&header);
    let block = NumHash::new(block_number, block_hash);
    let receipts = vec![make_test_receipt(
        block_number,
        block_hash,
        B256::with_last_byte(0x01),
        0,
        21_000,
        Bloom::ZERO,
    )];

    let err = verify_receipts(block, B256::with_last_byte(0xff), Bloom::ZERO, &receipts)
        .expect_err("mismatched receipts root should fail");

    assert!(
        err.to_string().contains("receipt root mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn verify_receipts_rejects_changed_receipt_bloom() {
    let block_number = 42;
    let header = make_test_header(block_number);
    let block_hash = header_hash(&header);
    let block = NumHash::new(block_number, block_hash);
    let receipts = vec![make_test_receipt(
        block_number,
        block_hash,
        B256::with_last_byte(0x01),
        0,
        21_000,
        Bloom::ZERO,
    )];
    let receipts_root = calculate_test_receipts_root(&receipts);
    let mut tampered_receipts = receipts;
    tampered_receipts[0].inner.inner.logs_bloom = Bloom::repeat_byte(0x01);

    let err = verify_receipts(block, receipts_root, Bloom::ZERO, &tampered_receipts)
        .expect_err("tampered receipt bloom should fail");

    assert!(
        err.to_string().contains("receipt root mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn verify_receipts_rejects_logs_bloom_mismatch() {
    let block_number = 42;
    let header = make_test_header(block_number);
    let block_hash = header_hash(&header);
    let block = NumHash::new(block_number, block_hash);
    let receipts = vec![make_test_receipt(
        block_number,
        block_hash,
        B256::with_last_byte(0x01),
        0,
        21_000,
        Bloom::repeat_byte(0x01),
    )];
    let receipts_root = calculate_test_receipts_root(&receipts);

    let err = verify_receipts(block, receipts_root, Bloom::ZERO, &receipts)
        .expect_err("mismatched header logs bloom should fail");

    assert!(
        err.to_string().contains("logs bloom mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn tempo_header_rejects_trailing_bytes_after_outer_list() {
    let fixture = malformed_tempo_headers_fixture();
    assert_tempo_header_fixture_rejected(&fixture.trailing_bytes_after_outer_list);
}

#[test]
fn tempo_header_rejects_outer_list_length_mismatch() {
    let fixture = malformed_tempo_headers_fixture();
    assert_tempo_header_fixture_rejected(&fixture.outer_list_length_mismatch);
}

#[test]
fn tempo_header_rejects_outer_list_long_length_leading_zero() {
    let fixture = malformed_tempo_headers_fixture();
    assert_tempo_header_fixture_rejected(&fixture.outer_list_long_length_leading_zero);
}

#[test]
fn tempo_header_rejects_difficulty_non_canonical_short_string() {
    let fixture = malformed_tempo_headers_fixture();
    assert_tempo_header_fixture_rejected(&fixture.difficulty_non_canonical_short_string);
}

#[test]
fn tempo_header_rejects_block_number_leading_zero() {
    let fixture = malformed_tempo_headers_fixture();
    assert_tempo_header_fixture_rejected(&fixture.block_number_leading_zero);
}

#[test]
fn tempo_header_rejects_extra_data_long_length_below_short_threshold() {
    let fixture = malformed_tempo_headers_fixture();
    assert_tempo_header_fixture_rejected(&fixture.extra_data_long_length_below_short_threshold);
}

fn assert_tempo_header_fixture_rejected(value: &str) {
    let malformed = parse_fixture_hex(value);

    assert_tempo_header_rejected(&malformed);
}

fn assert_tempo_header_rejected(input: &[u8]) {
    let mut buf = input;
    let decoded = <TempoHeader as alloy_rlp::Decodable>::decode(&mut buf);
    assert!(
        decoded.is_err() || !buf.is_empty(),
        "TempoHeader should reject malformed RLP input 0x{}",
        const_hex::encode(input)
    );
}

#[test]
fn update_l1_state_anchor_applies_raw_mutations_before_publishing_coverage() {
    let subscriber = test_subscriber(
        Arc::new(SequenceLocalTempoCheckpointReader::new([0])),
        Some(0),
    );
    let slot = B256::with_last_byte(1);
    let value = B256::with_last_byte(2);
    let stable_account = address!("0x0000000000000000000000000000000000000ABC");
    let stable_slot = B256::with_last_byte(3);
    let stable_value = B256::with_last_byte(4);
    subscriber
        .config
        .l1_state_cache
        .lock()
        .invalidate_and_set_anchor(9, []);
    subscriber
        .config
        .l1_state_cache
        .lock()
        .set(TIP403_REGISTRY_ADDRESS, slot, 10, value);
    subscriber
        .config
        .l1_state_cache
        .lock()
        .set(stable_account, stable_slot, 10, stable_value);

    subscriber.update_l1_state_anchor(10, &HashSet::new());
    assert_eq!(
        subscriber
            .config
            .l1_state_cache
            .lock()
            .get(TIP403_REGISTRY_ADDRESS, slot, 10),
        Some(value)
    );

    subscriber.update_l1_state_anchor(11, &HashSet::from([TIP403_REGISTRY_ADDRESS]));
    let mut cache = subscriber.config.l1_state_cache.lock();
    assert_eq!(
        cache.get(stable_account, stable_slot, 11),
        Some(stable_value)
    );
    assert_eq!(cache.get(TIP403_REGISTRY_ADDRESS, slot, 11), None);
}

/// Confirm the front of a shared `DepositQueue`, panicking if it fails.
fn confirm_shared(queue: &DepositQueue) -> L1BlockDeposits {
    let num_hash = queue.peek().expect("queue is empty").header.num_hash();
    queue.confirm(num_hash).expect("confirm mismatch")
}

fn deposit_hash_chain(previous_hash: B256, deposits: &[L1Deposit]) -> B256 {
    deposits.iter().fold(previous_hash, |current, deposit| {
        deposit.hash_chain(current)
    })
}

#[tokio::test]
async fn test_resolve_start_block_reads_live_local_state_each_time() {
    let subscriber = test_subscriber(
        Arc::new(SequenceLocalTempoCheckpointReader::new(VecDeque::from([
            10, 11,
        ]))),
        None,
    );
    assert_eq!(subscriber.resolve_start_block().await.unwrap(), Some(11));
    assert_eq!(subscriber.resolve_start_block().await.unwrap(), Some(12));
}

#[tokio::test]
async fn test_resolve_start_block_falls_back_to_genesis_override_when_local_state_is_zero() {
    let subscriber = test_subscriber(
        Arc::new(SequenceLocalTempoCheckpointReader::new(VecDeque::from([0]))),
        Some(42),
    );
    assert_eq!(subscriber.resolve_start_block().await.unwrap(), Some(43));
}

#[tokio::test]
async fn test_resolve_start_block_skips_backfill_without_checkpoint() {
    let subscriber = test_subscriber(
        Arc::new(SequenceLocalTempoCheckpointReader::new(VecDeque::from([0]))),
        None,
    );

    assert_eq!(subscriber.resolve_start_block().await.unwrap(), None);
}

#[tokio::test]
async fn test_follow_finalized_uses_new_heads_to_sync_missing_finalized_range() {
    let subscriber = test_subscriber(Arc::new(SequenceLocalTempoCheckpointReader::new([9])), None);
    let asserter = Asserter::new();
    let l1_provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter.clone());

    let header_10 = make_test_header(10);
    let header_11 = make_chained_header(11, header_hash(&header_10));
    let header_12 = make_chained_header(12, header_hash(&header_11));

    // Initial sync through finalized block 10.
    asserter.push_success(&Some(header_response(header_10.clone())));
    push_header_and_empty_receipts(&asserter, header_10);

    // One newHeads notification wakes the subscriber. The finalized tag has
    // advanced by two blocks, so both missing blocks must be ingested.
    asserter.push_success(&Some(header_response(header_12.clone())));
    push_header_and_empty_receipts(&asserter, header_11);
    push_header_and_empty_receipts(&asserter, header_12);

    let err = subscriber
        .follow_finalized(
            &l1_provider,
            futures::stream::iter([Ok::<_, eyre::Report>(())]),
        )
        .await
        .expect_err("finite trigger stream should end the subscriber");
    assert!(err.to_string().contains("head notification stream ended"));

    let DepositSink::Queue(queue) = &subscriber.deposit_sink else {
        panic!("test subscriber must retain deposits");
    };
    let blocks = queue.drain();
    assert_eq!(
        blocks
            .iter()
            .map(|block| block.header.number())
            .collect::<Vec<_>>(),
        vec![10, 11, 12]
    );
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn observer_advances_caches_without_retaining_deposit_blocks() {
    let subscriber = test_observer(Arc::new(SequenceLocalTempoCheckpointReader::new([9])), None);
    let cached_address = address!("0x0000000000000000000000000000000000000ABC");
    let cached_slot = B256::with_last_byte(1);
    let cached_value = B256::with_last_byte(2);
    {
        let mut cache = subscriber.config.l1_state_cache.lock();
        cache.invalidate_and_set_anchor(9, []);
        cache.set(cached_address, cached_slot, 9, cached_value);
    }

    let asserter = Asserter::new();
    let l1_provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter.clone());
    let header_10 = make_test_header(10);
    let header_11 = make_chained_header(11, header_hash(&header_10));
    let header_12 = make_chained_header(12, header_hash(&header_11));

    asserter.push_success(&Some(header_response(header_12.clone())));
    push_header_and_empty_receipts(&asserter, header_10);
    push_header_and_empty_receipts(&asserter, header_11);
    push_header_and_empty_receipts(&asserter, header_12);

    assert_eq!(
        subscriber
            .sync_finalized_once(&l1_provider, 10)
            .await
            .unwrap(),
        13
    );

    assert_eq!(
        subscriber
            .config
            .l1_state_cache
            .lock()
            .get(cached_address, cached_slot, 12),
        Some(cached_value),
        "receipt coverage must keep advancing on an observer"
    );
    assert_eq!(subscriber.config.block_tracker.latest().unwrap().number, 12);
    assert_eq!(
        subscriber.config.block_tracker.observed_hash(12),
        None,
        "an observer has no downstream consumer requiring retained observations"
    );
    assert!(
        matches!(subscriber.deposit_sink, DepositSink::Observer),
        "an observer must not accumulate finalized blocks in a deposit queue"
    );
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn test_head_triggers_falls_back_to_http_block_filter() {
    let subscriber = test_subscriber(
        Arc::new(SequenceLocalTempoCheckpointReader::new([10])),
        None,
    );
    let asserter = Asserter::new();
    let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect_mocked_client(asserter.clone())
        .erased();

    asserter.push_success(&U256::from(1));
    asserter.push_success(&vec![B256::with_last_byte(1)]);

    let mut triggers = subscriber.head_triggers(&l1_provider).await.unwrap();
    let trigger = tokio::time::timeout(Duration::from_secs(2), triggers.next())
        .await
        .expect("HTTP block filter should emit a trigger")
        .expect("HTTP block filter stream should remain open");

    trigger.expect("HTTP block filter request should succeed");
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn test_sync_finalized_once_does_not_refetch_current_cursor() {
    let subscriber = test_subscriber(
        Arc::new(SequenceLocalTempoCheckpointReader::new([10])),
        None,
    );
    let asserter = Asserter::new();
    let l1_provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter.clone());
    asserter.push_success(&Some(header_response(make_test_header(10))));

    let next = subscriber
        .sync_finalized_once(&l1_provider, 11)
        .await
        .unwrap();

    assert_eq!(next, 11);
    let DepositSink::Queue(queue) = &subscriber.deposit_sink else {
        panic!("test subscriber must retain deposits");
    };
    assert!(queue.drain().is_empty());
    assert!(asserter.read_q().is_empty());
}

#[test]
fn test_push_log_decodes_bounce_back_as_regular_deposit() {
    let portal_address = address!("0x0000000000000000000000000000000000000ABC");
    let fallback_nonce = 0xF1;
    let encoded_fallback_nonce = address!("0x00000000000000000000000000000000000000F1");
    let token = address!("0x0000000000000000000000000000000000002000");
    let event = WithdrawalBounceBack {
        newCurrentDepositQueueHash: B256::with_last_byte(0x42),
        fallbackNonce: fallback_nonce,
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
    assert_eq!(deposit.to, encoded_fallback_nonce);
    assert_eq!(deposit.amount, event.amount);
    assert_eq!(deposit.fee, 0, "bounce-back deposits should be fee-free");
    assert_eq!(
        deposit.memo,
        B256::ZERO,
        "bounce-back deposits should clear memo"
    );
}

#[test]
fn confirmed_token_enabled_event_updates_registry() {
    let subscriber = test_subscriber(
        Arc::new(SequenceLocalTempoCheckpointReader::new(VecDeque::from([0]))),
        None,
    );
    let token = address!("0x20c0000000000000000000000000000000000001");
    let events = L1PortalEvents {
        enabled_tokens: vec![EnabledToken {
            token,
            name: "Path USD".to_owned(),
            symbol: "pathUSD".to_owned(),
            currency: "USD".to_owned(),
        }],
        ..Default::default()
    };

    subscriber.apply_enabled_token_events(&events);

    assert!(subscriber.config.enabled_tokens.read().contains(&token));
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
        tempo_refund_recipient: address!("0x0000000000000000000000000000000000000001"),
        memo: B256::ZERO,
    });

    let d2 = L1Deposit::Regular(Deposit {
        token: address!("0x0000000000000000000000000000000000001000"),
        sender: address!("0x0000000000000000000000000000000000000003"),
        to: address!("0x0000000000000000000000000000000000000004"),
        amount: 200,
        fee: 0,
        tempo_refund_recipient: address!("0x0000000000000000000000000000000000000003"),
        memo: B256::ZERO,
    });

    let h10 = make_test_header(10);
    let h10_hash = header_hash(&h10);
    queue.enqueue(h10, L1PortalEvents::from_deposits(vec![d1]));
    queue.enqueue(
        make_chained_header(11, h10_hash),
        L1PortalEvents::from_deposits(vec![d2]),
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
    let fixture = encrypted_deposit_hash_fixture();
    let encrypted = fixture.deposit.to_l1_deposit();
    let previous_hash = parse_fixture_b256(&fixture.previous_hash);

    let next_hash = deposit_hash_chain(previous_hash, &[L1Deposit::Encrypted(encrypted.clone())]);

    let abi_encrypted = abi::EncryptedDeposit {
        token: encrypted.token,
        sender: encrypted.sender,
        amount: encrypted.amount,
        tempoRefundRecipient: encrypted.tempo_refund_recipient,
        keyIndex: encrypted.key_index,
        encrypted: abi::EncryptedDepositPayload {
            ephemeralPubkeyX: encrypted.ephemeral_pubkey_x,
            ephemeralPubkeyYParity: encrypted.ephemeral_pubkey_y_parity,
            ciphertext: encrypted.ciphertext.clone().into(),
            nonce: encrypted.nonce.into(),
            tag: encrypted.tag.into(),
        },
    };
    let expected = parse_fixture_b256(&fixture.expected_hash);
    let tuple_value_hash =
        keccak256((DepositType::Encrypted, abi_encrypted, previous_hash).abi_encode());
    let single_value_tuple_hash = parse_fixture_b256(&fixture.single_value_tuple_hash);

    assert_eq!(
        next_hash, expected,
        "encrypted deposit hash chain must match Solidity DepositQueueLib.enqueueEncrypted"
    );
    assert_eq!(
        tuple_value_hash, single_value_tuple_hash,
        "fixture should document the previous single-value tuple encoding"
    );
    assert_ne!(
        expected, tuple_value_hash,
        "single-value tuple encoding should not match Solidity abi.encode(...) for dynamic encrypted deposits"
    );
    assert_ne!(next_hash, B256::ZERO, "hash should be non-zero");
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
        tempo_refund_recipient: sender,
        memo: B256::ZERO,
    };

    let encrypted = EncryptedDeposit {
        token,
        sender,
        amount: 300_000,
        fee: 0,
        tempo_refund_recipient: sender,
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

    let next_hash = deposit_hash_chain(B256::ZERO, &deposits);

    // Manually compute expected chain
    let hash_1 = keccak256(
        (
            DepositType::Regular,
            abi::Deposit {
                token: regular.token,
                sender: regular.sender,
                to: regular.to,
                amount: regular.amount,
                tempoRefundRecipient: regular.tempo_refund_recipient,
                memo: regular.memo,
            },
            B256::ZERO,
        )
            .abi_encode_params(),
    );

    let hash_2 = keccak256(
        (
            DepositType::Encrypted,
            abi::EncryptedDeposit {
                token: encrypted.token,
                sender: encrypted.sender,
                amount: encrypted.amount,
                tempoRefundRecipient: encrypted.tempo_refund_recipient,
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
            .abi_encode_params(),
    );

    assert_eq!(next_hash, hash_2);
}

#[tokio::test]
async fn test_prepare_decrypted_deposit_defers_policy_to_upstream_mint() {
    use k256::{AffinePoint, ProjectivePoint, Scalar};

    let token = address!("0x0000000000000000000000000000000000001000");
    let sender = address!("0x0000000000000000000000000000000000001234");
    let recipient = address!("0x000000000000000000000000000000000000BEEF");
    let portal = address!("0x0000000000000000000000000000000000000ABC");
    let block_number = 10;

    let sequencer_key = k256::SecretKey::from_slice(&[0x11; 32]).expect("valid key");
    let seq_scalar: Scalar = *sequencer_key.to_nonzero_scalar();
    let seq_pub = AffinePoint::from(ProjectivePoint::GENERATOR * seq_scalar);
    let (seq_pub_x, seq_pub_y_parity) =
        crate::precompiles::ecies::compressed_x_and_parity(&seq_pub);
    let encrypted = crate::precompiles::ecies::encrypt_deposit(
        &seq_pub_x,
        seq_pub_y_parity,
        recipient,
        B256::ZERO,
        portal,
        U256::ZERO,
    )
    .expect("encrypted deposit should be valid");

    let block = L1BlockDeposits {
        header: seal(make_test_header(block_number)),
        events: L1PortalEvents::from_deposits(vec![L1Deposit::Encrypted(EncryptedDeposit {
            token,
            sender,
            amount: 1_000_000,
            fee: 0,
            tempo_refund_recipient: sender,
            key_index: U256::ZERO,
            ephemeral_pubkey_x: encrypted.eph_pub_x,
            ephemeral_pubkey_y_parity: encrypted.eph_pub_y_parity,
            ciphertext: encrypted.ciphertext,
            nonce: encrypted.nonce,
            tag: encrypted.tag,
        })]),
    };

    let prepared = block
        .prepare(&sequencer_key, portal)
        .await
        .expect("decrypted deposit should prepare without an engine-side policy read");

    assert_eq!(prepared.queued_deposits.len(), 1);
    assert_eq!(
        prepared.queued_deposits[0].depositType,
        DepositType::Encrypted
    );
    assert_eq!(
        prepared.decryptions.len(),
        1,
        "successfully decrypted deposits must provide on-chain decryption data"
    );
}

#[test]
fn finalized_queue_tracks_tip_after_consumption() {
    let queue = DepositQueue::new();
    assert!(queue.last_enqueued().is_none());

    let h100 = make_test_header(100);
    let h100_hash = header_hash(&h100);
    queue.enqueue(h100, L1PortalEvents::default());

    let h101 = make_chained_header(101, h100_hash);
    let h101_hash = header_hash(&h101);
    queue.enqueue(h101, L1PortalEvents::default());

    confirm_shared(&queue);
    confirm_shared(&queue);
    assert!(queue.peek().is_none());
    assert_eq!(
        queue.last_enqueued(),
        Some(NumHash::new(101, h101_hash)),
        "the subscriber high-water mark must survive consumption"
    );

    queue.enqueue(
        make_chained_header(102, h101_hash),
        L1PortalEvents::default(),
    );
}

#[test]
fn external_enqueue_reports_discontinuity_without_panicking() {
    let queue = DepositQueue::new();
    let h10 = make_test_header(10);
    let h10_hash = header_hash(&h10);
    assert!(
        queue
            .try_enqueue_sealed(seal(h10), L1PortalEvents::default())
            .unwrap()
    );

    let err = queue
        .try_enqueue_sealed(
            seal(make_chained_header(12, h10_hash)),
            L1PortalEvents::default(),
        )
        .expect_err("external input must report a gap");
    assert!(
        err.to_string()
            .contains("non-contiguous finalized L1 block")
    );
}

#[test]
fn external_enqueue_accepts_duplicate_producers() {
    let queue = DepositQueue::new();
    let h10 = make_test_header(10);
    let h11 = make_chained_header(11, header_hash(&h10));
    let h12 = make_chained_header(12, header_hash(&h11));
    let duplicate = seal(h10);
    assert!(
        queue
            .try_enqueue_sealed(duplicate.clone(), L1PortalEvents::default())
            .unwrap()
    );
    for header in [h11, h12] {
        queue
            .try_enqueue_sealed(seal(header), L1PortalEvents::default())
            .unwrap();
    }
    assert!(
        !queue
            .try_enqueue_sealed(duplicate, L1PortalEvents::default())
            .unwrap()
    );
    assert_eq!(queue.peek().unwrap().header.number(), 10);
}

#[test]
fn confirm_through_is_idempotent_and_drains_stale_entries() {
    let queue = DepositQueue::new();
    let h10 = make_test_header(10);
    let h11 = make_chained_header(11, header_hash(&h10));
    let h12 = make_chained_header(12, header_hash(&h11));
    let anchor = seal(h12.clone()).num_hash();
    for header in [h10, h11, h12] {
        queue
            .try_enqueue_sealed(seal(header), L1PortalEvents::default())
            .unwrap();
    }

    queue.confirm_through(anchor).unwrap();
    assert!(queue.peek().is_none());
    queue.confirm_through(anchor).unwrap();
}

#[test]
fn confirm_through_rejects_a_conflicting_anchor() {
    let queue = DepositQueue::new();
    queue
        .try_enqueue_sealed(seal(make_test_header(10)), L1PortalEvents::default())
        .unwrap();

    let err = queue
        .confirm_through(NumHash::new(10, B256::repeat_byte(0xab)))
        .expect_err("a different hash at the same height must fail");
    assert!(err.to_string().contains("deposit queue holds L1 block 10"));
    assert_eq!(queue.peek().unwrap().header.number(), 10);
}

#[test]
#[should_panic(expected = "finalized L1 queue invariant violated")]
fn finalized_deposit_queue_panics_on_discontinuity() {
    let queue = DepositQueue::new();
    let h10 = make_test_header(10);
    let h10_hash = header_hash(&h10);
    queue.enqueue(h10, L1PortalEvents::default());
    queue.enqueue(make_chained_header(12, h10_hash), L1PortalEvents::default());
}

#[test]
fn finalized_queue_accepts_only_contiguous_blocks() {
    let mut queue = PendingDeposits::default();
    let h10 = make_test_header(10);
    let h10_hash = header_hash(&h10);

    assert!(
        queue
            .try_enqueue(seal(h10), L1PortalEvents::default())
            .expect("first finalized block should enqueue")
    );
    assert!(
        queue
            .try_enqueue(
                seal(make_chained_header(11, h10_hash)),
                L1PortalEvents::default(),
            )
            .expect("child finalized block should enqueue")
    );
    assert_eq!(queue.pending_len(), 2);
}

#[test]
fn finalized_queue_tip_redelivery_is_idempotent() {
    let mut queue = PendingDeposits::default();
    let h10 = make_test_header(10);

    assert!(
        queue
            .try_enqueue(seal(h10.clone()), L1PortalEvents::default())
            .expect("first finalized block should enqueue")
    );
    assert!(
        !queue
            .try_enqueue(seal(h10), L1PortalEvents::default())
            .expect("exact tip redelivery should be accepted")
    );
    assert_eq!(queue.pending_len(), 1);
}

#[test]
fn finalized_queue_rejects_gap_without_mutation() {
    let mut queue = PendingDeposits::default();
    let h10 = make_test_header(10);
    let h10_hash = header_hash(&h10);
    queue.enqueue(h10, L1PortalEvents::default());

    let err = queue
        .try_enqueue(
            seal(make_chained_header(12, h10_hash)),
            L1PortalEvents::default(),
        )
        .expect_err("a finalized height gap must be fatal");

    assert!(
        err.to_string()
            .contains("non-contiguous finalized L1 block")
    );
    assert_eq!(queue.pending_len(), 1);
    assert_eq!(queue.last_enqueued(), Some(NumHash::new(10, h10_hash)));
}

#[test]
fn finalized_queue_rejects_parent_mismatch_without_mutation() {
    let mut queue = PendingDeposits::default();
    let h10 = make_test_header(10);
    let h10_hash = header_hash(&h10);
    queue.enqueue(h10, L1PortalEvents::default());

    let err = queue
        .try_enqueue(
            seal(make_chained_header(11, B256::with_last_byte(0xff))),
            L1PortalEvents::default(),
        )
        .expect_err("a finalized parent mismatch must be fatal");

    assert!(err.to_string().contains("finalized L1 parent mismatch"));
    assert_eq!(queue.pending_len(), 1);
    assert_eq!(queue.last_enqueued(), Some(NumHash::new(10, h10_hash)));
}

#[test]
fn finalized_queue_rejects_conflicting_tip_without_mutation() {
    let mut queue = PendingDeposits::default();
    let h10 = make_test_header(10);
    let h10_hash = header_hash(&h10);
    queue.enqueue(h10, L1PortalEvents::default());

    let mut conflicting = make_test_header(10);
    conflicting.inner.gas_limit += 1;
    let err = queue
        .try_enqueue(seal(conflicting), L1PortalEvents::default())
        .expect_err("a conflicting finalized tip must be fatal");

    assert!(err.to_string().contains("conflicting finalized L1 block"));
    assert_eq!(queue.pending_len(), 1);
    assert_eq!(queue.last_enqueued(), Some(NumHash::new(10, h10_hash)));
}

#[test]
fn finalized_queue_rejects_stale_blocks() {
    let mut queue = PendingDeposits::default();
    queue.enqueue(make_test_header(10), L1PortalEvents::default());

    let err = queue
        .try_enqueue(seal(make_test_header(9)), L1PortalEvents::default())
        .expect_err("an out-of-order finalized block must be fatal");

    assert!(err.to_string().contains("out-of-order finalized L1 block"));
    assert_eq!(queue.pending_len(), 1);
}

#[test]
fn finalized_queue_rejects_confirmation_mismatch_without_mutation() {
    let mut queue = PendingDeposits::default();
    let h10 = make_test_header(10);
    let h10_hash = header_hash(&h10);
    queue.enqueue(h10, L1PortalEvents::default());

    let err = queue
        .confirm(NumHash::new(10, B256::with_last_byte(0xff)))
        .expect_err("confirming a different block must fail");

    assert!(
        err.to_string()
            .contains("finalized L1 queue confirmation mismatch")
    );
    assert_eq!(queue.pending_len(), 1);
    assert_eq!(
        queue
            .peek()
            .expect("front should remain queued")
            .header
            .hash(),
        h10_hash
    );
}
