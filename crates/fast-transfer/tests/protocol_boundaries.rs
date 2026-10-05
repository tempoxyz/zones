//! Deterministic protocol boundary acceptance tests.
//!
//! These tests exercise the service's public state/accounting contracts. They do not replace
//! replica, storage, precompile, or E2E tests.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use zone_fast_transfer::{
    AdmissionController, AdmissionDecision, DeliveryRecord, DeliveryTransition, DestinationRecord,
    ProtocolLimits, ReplenishmentJob, ReplenishmentStage, SourceRecord,
    admission::{PoolAccounting, RouteKey, ValueCaps},
    replenishment::{DepositRecord, InventoryContribution, WithdrawalRecord},
    state::{DestinationDecision, SourceStage},
};
use zone_primitives::fast_transfer::{AssetId, RejectionReason, TransferIntent, ZoneDomain};

fn address(byte: u8) -> Address {
    Address::from([byte; 20])
}

fn hash(byte: u8) -> B256 {
    B256::from([byte; 32])
}

fn domain(zone_id: u32, marker: u8) -> ZoneDomain {
    ZoneDomain {
        l1_chain_id: 1_337,
        zone_id,
        chain_id: 10_000 + u64::from(zone_id),
        portal: address(marker),
        authority_epoch: 9,
        roster_hash: hash(marker),
        protocol_version: 1,
    }
}

fn intent(sender: Address, nonce: u64, principal: u64) -> TransferIntent {
    TransferIntent {
        source: domain(1, 0x11),
        destination: domain(2, 0x22),
        asset: AssetId {
            l1_token: address(0x31),
            source_token: address(0x32),
            destination_token: address(0x33),
            decimals: 6,
        },
        sender,
        recipient: address(0x42),
        refund_account: sender,
        destination_pool: address(0x43),
        reimbursement_account: address(0x44),
        principal: U256::from(principal),
        fee: U256::from(7_u64),
        quote_id: hash(0x51),
        destination_expiry_height: 1_000,
        transfer_nonce: nonce,
    }
}

#[test]
fn deployment_defaults_match_every_specified_resource_limit() {
    assert_eq!(
        ProtocolLimits::default(),
        ProtocolLimits {
            unresolved_zone: 10_000,
            unresolved_route: 1_000,
            unresolved_sender: 100,
            queued_zone_bytes: 64 * 1024 * 1024,
            queued_peer_bytes: 8 * 1024 * 1024,
            journal_bytes: 1024 * 1024 * 1024,
            verification_concurrency_per_peer: 32,
        }
    );
}

#[test]
fn duplicate_reservation_consumes_no_count_bytes_or_value_and_conflicting_body_fails() {
    let limits = ProtocolLimits {
        unresolved_zone: 1,
        unresolved_route: 1,
        unresolved_sender: 1,
        queued_zone_bytes: 100,
        queued_peer_bytes: 100,
        journal_bytes: 100,
        verification_concurrency_per_peer: 1,
    };
    let transfer = intent(address(0x41), 1, 10);
    let route = RouteKey::from_intent(&transfer);
    let mut admission = AdmissionController::new(limits);
    admission.set_value_caps(
        route,
        ValueCaps {
            route: U256::from(10_u64),
            sender: U256::from(10_u64),
        },
    );

    assert_eq!(
        admission.reserve(&transfer, 100, 100).unwrap(),
        AdmissionDecision::Accepted
    );
    assert_eq!(
        admission.reserve(&transfer, 100, 100).unwrap(),
        AdmissionDecision::Duplicate
    );
    assert_eq!(admission.unresolved(), 1);

    let conflicting = TransferIntent {
        recipient: address(0x99),
        ..transfer
    };
    assert_eq!(conflicting.transfer_id(), transfer.transfer_id());
    assert!(admission.reserve(&conflicting, 0, 0).is_err());

    admission.release(transfer.transfer_id()).unwrap();
    assert_eq!(admission.unresolved(), 0);
    assert_eq!(
        admission.reserve(&transfer, 100, 100).unwrap(),
        AdmissionDecision::Accepted
    );
}

#[test]
fn each_count_byte_value_and_verification_limit_fails_closed_at_the_boundary() {
    let limits = ProtocolLimits {
        unresolved_zone: 2,
        unresolved_route: 2,
        unresolved_sender: 1,
        queued_zone_bytes: 20,
        queued_peer_bytes: 10,
        journal_bytes: 20,
        verification_concurrency_per_peer: 2,
    };
    let alice_one = intent(address(0x41), 1, 10);
    let alice_two = intent(address(0x41), 2, 10);
    let bob = intent(address(0x45), 3, 11);
    let route = RouteKey::from_intent(&alice_one);
    let mut admission = AdmissionController::new(limits);
    admission.set_value_caps(
        route,
        ValueCaps {
            route: U256::from(20_u64),
            sender: U256::from(10_u64),
        },
    );

    assert_eq!(
        admission.reserve(&alice_one, 10, 10).unwrap(),
        AdmissionDecision::Accepted
    );
    assert!(
        admission.reserve(&alice_two, 1, 1).is_err(),
        "sender count/value cap must stop admission"
    );
    assert!(
        admission.reserve(&bob, 1, 1).is_err(),
        "route value cap must stop admission"
    );

    assert!(admission.reserve_peer_bytes(9, 10).is_ok());
    assert!(
        admission.reserve_peer_bytes(9, 1).is_err(),
        "per-peer bytes exceeded"
    );
    assert!(
        admission.reserve_peer_bytes(10, 1).is_err(),
        "global queue bytes exceeded"
    );
    admission.release_peer_bytes(9, 10).unwrap();

    assert!(admission.acquire_verification(9).is_ok());
    assert!(admission.acquire_verification(9).is_ok());
    assert!(admission.acquire_verification(9).is_err());
    admission.release_verification(9).unwrap();
    assert!(admission.acquire_verification(9).is_ok());
}

#[test]
fn pool_uses_only_funded_liquidity_preserves_reserve_and_retires_once() {
    let asset = intent(address(0x41), 1, 10).asset;
    let mut pool = PoolAccounting::new(asset, U256::from(100_u64), U256::from(10_u64)).unwrap();
    pool.set_exposure_cap(1, U256::from(50_u64));
    assert_eq!(pool.spendable(), U256::from(90_u64));

    let first = hash(0x61);
    pool.commit_payment(first, 1, U256::from(40_u64)).unwrap();
    assert_eq!(pool.funded_balance(), U256::from(60_u64));
    assert_eq!(pool.spendable(), U256::from(50_u64));

    let before_duplicate = pool.funded_balance();
    assert!(pool.commit_payment(first, 1, U256::from(40_u64)).is_err());
    assert_eq!(
        pool.funded_balance(),
        before_duplicate,
        "duplicate ID debited pool again"
    );
    assert!(
        pool.commit_payment(hash(0x62), 1, U256::from(11_u64))
            .is_err()
    );

    assert!(pool.retire_exposure(first, 1, U256::from(40_u64)).unwrap());
    assert!(!pool.retire_exposure(first, 1, U256::from(40_u64)).unwrap());
    assert_eq!(
        pool.funded_balance(),
        U256::from(60_u64),
        "retirement is not pool funding"
    );

    pool.credit_observed(U256::from(40_u64)).unwrap();
    assert_eq!(pool.funded_balance(), U256::from(100_u64));
}

#[test]
fn destination_choice_and_source_disposition_are_immutable_and_idempotent() {
    let transfer = intent(address(0x41), 1, 10);
    let paid = DestinationDecision::Paid {
        pool: transfer.destination_pool,
        recipient: transfer.recipient,
        principal: transfer.principal,
    };
    let rejected = DestinationDecision::Rejected(RejectionReason::Cancelled);

    let mut destination = DestinationRecord::new(&transfer);
    assert!(destination.check_intent(&transfer).is_ok());
    assert!(destination.decide(paid).unwrap());
    assert!(!destination.decide(paid).unwrap());
    assert!(destination.decide(rejected).is_err());

    let conflicting = TransferIntent {
        recipient: address(0x99),
        ..transfer
    };
    assert!(destination.check_intent(&conflicting).is_err());

    let mut source = SourceRecord::locked(&transfer).unwrap();
    assert_eq!(source.escrow_amount, U256::from(17_u64));
    assert_eq!(source.stage, SourceStage::Locked);
    assert!(source.complete_disposition().is_err());
    assert!(source.record_decision(paid).unwrap());
    assert!(!source.record_decision(paid).unwrap());
    assert!(source.record_decision(rejected).is_err());
    assert!(source.complete_disposition().unwrap());
    assert_eq!(source.stage, SourceStage::Released);
    assert!(!source.complete_disposition().unwrap());
    assert!(source.record_decision(rejected).is_err());
}

#[test]
fn delivery_ack_is_not_disposition_and_retry_is_bounded_from_fifty_ms_to_two_seconds() {
    let mut record = DeliveryRecord::queued(7, 11, hash(0x71), vec![1, 2, 3]);
    assert!(!record.compactable());
    assert!(
        record
            .advance(DeliveryTransition::TransportAcknowledged)
            .unwrap()
    );
    assert!(
        !record.compactable(),
        "transport acknowledgment compacted unresolved work"
    );
    assert!(
        record
            .advance(DeliveryTransition::TerminalReceived)
            .unwrap()
    );
    assert!(
        !record.compactable(),
        "terminal receipt compacted undisposed source work"
    );
    assert!(record.advance(DeliveryTransition::Queued).is_err());

    let expected = [50, 100, 200, 400, 800, 1_600, 2_000, 2_000];
    for millis in expected {
        assert_eq!(
            record.record_attempt(0).unwrap(),
            Duration::from_millis(millis)
        );
    }
    assert!(record.advance(DeliveryTransition::SourceDisposed).unwrap());
    assert!(record.compactable());
    assert!(!record.advance(DeliveryTransition::SourceDisposed).unwrap());
}

#[test]
fn replenishment_job_uses_permanent_identity_and_monotonic_two_leg_stages() {
    let source_inventory = address(0x82);
    let source_fallback = address(0x83);
    let treasury = address(0x84);
    let destination_pool = address(0x85);
    let fast_recipient = address(0x86);
    let contributions = vec![
        InventoryContribution {
            transfer_id: hash(0x87),
            amount: U256::from(4_u64),
        },
        InventoryContribution {
            transfer_id: hash(0x88),
            amount: U256::from(6_u64),
        },
    ];
    let mut job = ReplenishmentJob::allocate(
        hash(0x81),
        source_inventory,
        source_fallback,
        treasury,
        destination_pool,
        fast_recipient,
        contributions.clone(),
    )
    .unwrap();
    assert_eq!(job.stage, ReplenishmentStage::InventoryAllocated);
    assert_eq!(job.gross_amount, U256::from(10_u64));

    assert!(
        ReplenishmentJob::allocate(
            hash(0x89),
            fast_recipient,
            source_fallback,
            treasury,
            destination_pool,
            fast_recipient,
            contributions,
        )
        .is_err(),
        "background job reused the original fast recipient"
    );

    let withdrawal = WithdrawalRecord {
        transaction_intent_hash: hash(0x90),
        signer_nonce: 7,
        fallback_nonce: 8,
        transaction_hash: None,
        accepted_batch_hash: None,
        queue_index: None,
    };
    assert!(job.request_withdrawal(withdrawal.clone()).unwrap());
    assert!(!job.request_withdrawal(withdrawal).unwrap());
    job.reconcile_withdrawal(Some(hash(0x91)), Some(hash(0x92)), Some(3))
        .unwrap();
    assert!(job.record_treasury_credit(U256::from(9_u64)).unwrap());
    assert!(!job.record_treasury_credit(U256::from(9_u64)).unwrap());

    let deposit = DepositRecord {
        transaction_intent_hash: hash(0x93),
        signer_nonce: 11,
        queue_index: None,
        transaction_hash: None,
    };
    assert!(job.submit_deposit(deposit.clone()).unwrap());
    assert!(!job.submit_deposit(deposit).unwrap());
    job.reconcile_deposit(Some(hash(0x94)), Some(5)).unwrap();
    assert!(job.record_pool_credit(U256::from(8_u64)).unwrap());
    assert!(!job.record_pool_credit(U256::from(8_u64)).unwrap());
    assert_eq!(job.stage, ReplenishmentStage::PoolCredited);
    assert_eq!(job.pool_credit, Some(U256::from(8_u64)));
    assert!(job.record_treasury_credit(U256::from(9_u64)).is_err());
}
