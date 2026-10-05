use alloy_primitives::{Address, B256, U256};
use zone_fast_transfer::{
    AdmissionController, AdmissionDecision, AdmissionError, DestinationRecord, ProtocolLimits,
    SourceRecord, StateError,
    admission::{PoolAccounting, RouteKey, ValueCaps},
    state::{DestinationDecision, SourceStage},
};
use zone_primitives::fast_transfer::{AssetId, RejectionReason, TransferIntent, ZoneDomain};

fn intent(nonce: u64, sender: Address) -> TransferIntent {
    let source = ZoneDomain {
        l1_chain_id: 1,
        zone_id: 1,
        chain_id: 101,
        portal: Address::repeat_byte(1),
        authority_epoch: 4,
        roster_hash: B256::repeat_byte(1),
        protocol_version: 1,
    };
    let destination = ZoneDomain {
        zone_id: 2,
        chain_id: 102,
        portal: Address::repeat_byte(2),
        roster_hash: B256::repeat_byte(2),
        ..source
    };
    TransferIntent {
        source,
        destination,
        asset: AssetId {
            l1_token: Address::repeat_byte(3),
            source_token: Address::repeat_byte(4),
            destination_token: Address::repeat_byte(5),
            decimals: 6,
        },
        sender,
        recipient: Address::repeat_byte(6 + nonce as u8),
        refund_account: sender,
        destination_pool: Address::repeat_byte(20),
        reimbursement_account: Address::repeat_byte(21),
        principal: U256::from(10),
        fee: U256::from(1),
        quote_id: B256::repeat_byte(22),
        destination_expiry_height: 100,
        transfer_nonce: nonce,
    }
}

#[test]
fn liquidity_race_serializes_one_payment_and_one_refund_without_inflation() {
    let sender = Address::repeat_byte(30);
    let first = intent(1, sender);
    let second = intent(2, sender);
    let route = RouteKey::from_intent(&first);
    let mut admission = AdmissionController::new(ProtocolLimits {
        unresolved_zone: 2,
        unresolved_route: 2,
        unresolved_sender: 2,
        ..ProtocolLimits::default()
    });
    admission.set_value_caps(
        route,
        ValueCaps {
            route: U256::from(20),
            sender: U256::from(20),
        },
    );
    assert_eq!(
        admission.reserve(&first, 100, 200),
        Ok(AdmissionDecision::Accepted)
    );
    assert_eq!(
        admission.reserve(&second, 100, 200),
        Ok(AdmissionDecision::Accepted)
    );

    let mut pool = PoolAccounting::new(first.asset, U256::from(10), U256::ZERO).unwrap();
    pool.set_exposure_cap(first.source.zone_id, U256::from(20));
    let mut first_destination = DestinationRecord::new(&first);
    let mut second_destination = DestinationRecord::new(&second);
    let paid = DestinationDecision::Paid {
        pool: first.destination_pool,
        recipient: first.recipient,
        principal: first.principal,
    };
    pool.commit_payment(first.transfer_id(), first.source.zone_id, first.principal)
        .unwrap();
    first_destination.decide(paid).unwrap();
    assert_eq!(
        pool.commit_payment(
            second.transfer_id(),
            second.source.zone_id,
            second.principal
        ),
        Err(AdmissionError::Pool("insufficient spendable liquidity"))
    );
    let rejected = DestinationDecision::Rejected(RejectionReason::InsufficientLiquidity);
    second_destination.decide(rejected).unwrap();

    let mut first_source = SourceRecord::locked(&first).unwrap();
    let mut second_source = SourceRecord::locked(&second).unwrap();
    first_source.record_decision(paid).unwrap();
    second_source.record_decision(rejected).unwrap();
    first_source.complete_disposition().unwrap();
    second_source.complete_disposition().unwrap();

    assert_eq!(pool.funded_balance(), U256::ZERO);
    assert_eq!(first_source.stage, SourceStage::Released);
    assert_eq!(second_source.stage, SourceStage::Refunded);
    assert_eq!(first_destination.decision, Some(paid));
    assert_eq!(second_destination.decision, Some(rejected));
    assert_eq!(first.principal + pool.funded_balance(), U256::from(10));
}

#[test]
fn cancellation_and_payment_are_permanent_mutually_exclusive_outcomes() {
    let value = intent(7, Address::repeat_byte(40));
    let paid = DestinationDecision::Paid {
        pool: value.destination_pool,
        recipient: value.recipient,
        principal: value.principal,
    };
    let cancelled = DestinationDecision::Rejected(RejectionReason::Cancelled);

    let mut payment_wins = DestinationRecord::new(&value);
    assert_eq!(payment_wins.decide(paid), Ok(true));
    assert_eq!(
        payment_wins.decide(cancelled),
        Err(StateError::TerminalConflict)
    );

    let mut cancellation_wins = DestinationRecord::new(&value);
    assert_eq!(cancellation_wins.decide(cancelled), Ok(true));
    assert_eq!(
        cancellation_wins.decide(paid),
        Err(StateError::TerminalConflict)
    );
}

#[test]
fn policy_blocked_disposition_retains_the_original_beneficiary_liability() {
    let value = intent(9, Address::repeat_byte(50));
    let paid = DestinationDecision::Paid {
        pool: value.destination_pool,
        recipient: value.recipient,
        principal: value.principal,
    };
    let mut source = SourceRecord::locked(&value).unwrap();

    source.record_decision(paid).unwrap();
    assert_eq!(source.stage, SourceStage::PaidAwaitingRelease);
    assert_eq!(source.escrow_amount, value.principal + value.fee);
    assert_eq!(source.record_decision(paid), Ok(false));
    assert_eq!(source.stage, SourceStage::PaidAwaitingRelease);

    assert_eq!(source.complete_disposition(), Ok(true));
    assert_eq!(source.complete_disposition(), Ok(false));
    assert_eq!(source.stage, SourceStage::Released);
    assert_eq!(
        source.record_decision(DestinationDecision::Rejected(RejectionReason::PolicyDenied)),
        Err(StateError::DispositionConflict)
    );
}

#[test]
fn duplicate_admission_and_retirement_do_not_consume_capacity_twice() {
    let value = intent(11, Address::repeat_byte(60));
    let route = RouteKey::from_intent(&value);
    let mut admission = AdmissionController::new(ProtocolLimits {
        unresolved_zone: 1,
        unresolved_route: 1,
        unresolved_sender: 1,
        ..ProtocolLimits::default()
    });
    admission.set_value_caps(
        route,
        ValueCaps {
            route: value.principal,
            sender: value.principal,
        },
    );
    assert_eq!(
        admission.reserve(&value, 1, 1),
        Ok(AdmissionDecision::Accepted)
    );
    assert_eq!(
        admission.reserve(&value, 1, 1),
        Ok(AdmissionDecision::Duplicate)
    );
    let mut conflicting = value.clone();
    conflicting.recipient = Address::repeat_byte(99);
    assert_eq!(conflicting.transfer_id(), value.transfer_id());
    assert_eq!(
        admission.reserve(&conflicting, 1, 1),
        Err(AdmissionError::ConflictingIntent)
    );
    assert_eq!(admission.unresolved(), 1);

    let mut pool = PoolAccounting::new(value.asset, U256::from(20), U256::from(5)).unwrap();
    pool.set_exposure_cap(value.source.zone_id, value.principal);
    pool.commit_payment(value.transfer_id(), value.source.zone_id, value.principal)
        .unwrap();
    assert_eq!(pool.spendable(), U256::from(5));
    assert_eq!(
        pool.retire_exposure(value.transfer_id(), value.source.zone_id, value.principal),
        Ok(true)
    );
    assert_eq!(
        pool.retire_exposure(value.transfer_id(), value.source.zone_id, value.principal),
        Ok(false)
    );
    assert_eq!(pool.funded_balance(), U256::from(10));
}

#[test]
fn every_count_byte_and_verification_limit_refuses_before_overcommit() {
    let first = intent(21, Address::repeat_byte(70));
    let second = intent(22, Address::repeat_byte(71));
    let route = RouteKey::from_intent(&first);
    let caps = ValueCaps {
        route: U256::MAX,
        sender: U256::MAX,
    };

    let mut route_limited = AdmissionController::new(ProtocolLimits {
        unresolved_zone: 2,
        unresolved_route: 1,
        unresolved_sender: 2,
        ..ProtocolLimits::default()
    });
    route_limited.set_value_caps(route, caps);
    route_limited.reserve(&first, 0, 0).unwrap();
    assert_eq!(
        route_limited.reserve(&second, 0, 0),
        Err(AdmissionError::Limit("route unresolved count"))
    );

    let same_sender = intent(23, first.sender);
    let mut other_route = intent(24, first.sender);
    other_route.destination.zone_id = 3;
    other_route.destination.chain_id = 103;
    other_route.destination.portal = Address::repeat_byte(3);
    let mut sender_limited = AdmissionController::new(ProtocolLimits {
        unresolved_zone: 2,
        unresolved_route: 2,
        unresolved_sender: 1,
        ..ProtocolLimits::default()
    });
    sender_limited.set_value_caps(RouteKey::from_intent(&same_sender), caps);
    sender_limited.set_value_caps(RouteKey::from_intent(&other_route), caps);
    sender_limited.reserve(&same_sender, 0, 0).unwrap();
    assert_eq!(
        sender_limited.reserve(&other_route, 0, 0),
        Err(AdmissionError::Limit("sender unresolved count"))
    );

    let mut zone_limited = AdmissionController::new(ProtocolLimits {
        unresolved_zone: 1,
        unresolved_route: 2,
        unresolved_sender: 2,
        ..ProtocolLimits::default()
    });
    zone_limited.set_value_caps(route, caps);
    zone_limited.reserve(&first, 0, 0).unwrap();
    assert_eq!(
        zone_limited.reserve(&second, 0, 0),
        Err(AdmissionError::Limit("zone unresolved count"))
    );

    let mut bytes_limited = AdmissionController::new(ProtocolLimits {
        queued_zone_bytes: 10,
        queued_peer_bytes: 6,
        journal_bytes: 12,
        verification_concurrency_per_peer: 2,
        ..ProtocolLimits::default()
    });
    bytes_limited.set_value_caps(route, caps);
    assert_eq!(
        bytes_limited.reserve(&first, 11, 1),
        Err(AdmissionError::Limit("zone queue bytes"))
    );
    assert_eq!(
        bytes_limited.reserve(&first, 1, 13),
        Err(AdmissionError::Limit("journal bytes"))
    );
    bytes_limited.reserve_peer_bytes(2, 6).unwrap();
    assert_eq!(
        bytes_limited.reserve_peer_bytes(2, 1),
        Err(AdmissionError::Limit("peer queue bytes"))
    );
    bytes_limited.acquire_verification(2).unwrap();
    bytes_limited.acquire_verification(2).unwrap();
    assert_eq!(
        bytes_limited.acquire_verification(2),
        Err(AdmissionError::Limit("peer verification concurrency"))
    );
}

#[test]
fn route_and_sender_base_unit_caps_are_exact() {
    let first = intent(31, Address::repeat_byte(80));
    let second_sender = intent(32, Address::repeat_byte(81));
    let route = RouteKey::from_intent(&first);
    let mut route_capped = AdmissionController::new(ProtocolLimits::default());
    route_capped.set_value_caps(
        route,
        ValueCaps {
            route: first.principal,
            sender: U256::MAX,
        },
    );
    route_capped.reserve(&first, 0, 0).unwrap();
    assert_eq!(
        route_capped.reserve(&second_sender, 0, 0),
        Err(AdmissionError::Limit("route base-unit exposure"))
    );

    let mut other_route = intent(33, first.sender);
    other_route.destination.zone_id = 3;
    other_route.destination.chain_id = 103;
    other_route.destination.portal = Address::repeat_byte(3);
    let mut sender_capped = AdmissionController::new(ProtocolLimits::default());
    let sender_caps = ValueCaps {
        route: U256::MAX,
        sender: first.principal,
    };
    sender_capped.set_value_caps(route, sender_caps);
    sender_capped.set_value_caps(RouteKey::from_intent(&other_route), sender_caps);
    sender_capped.reserve(&first, 0, 0).unwrap();
    assert_eq!(
        sender_capped.reserve(&other_route, 0, 0),
        Err(AdmissionError::Limit("sender base-unit exposure"))
    );
}

#[test]
fn deployment_defaults_match_all_bounded_admission_requirements() {
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
