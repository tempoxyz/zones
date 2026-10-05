use alloy_primitives::{Address, B256, U256};
use zone_fast_transfer::{
    AdmissionError, DeliveryError, DeliveryRecord, DeliveryTransition, DestinationRecord,
    SourceRecord, StateError, admission::PoolAccounting, state::DestinationDecision,
};
use zone_primitives::fast_transfer::{AssetId, RejectionReason, TransferIntent, ZoneDomain};

fn intent(principal: U256, fee: U256) -> TransferIntent {
    let source = ZoneDomain {
        l1_chain_id: 1,
        zone_id: 1,
        chain_id: 101,
        portal: Address::repeat_byte(1),
        authority_epoch: 7,
        roster_hash: B256::repeat_byte(1),
        protocol_version: 1,
    };
    TransferIntent {
        source,
        destination: ZoneDomain {
            zone_id: 2,
            chain_id: 102,
            portal: Address::repeat_byte(2),
            roster_hash: B256::repeat_byte(2),
            ..source
        },
        asset: AssetId {
            l1_token: Address::repeat_byte(3),
            source_token: Address::repeat_byte(4),
            destination_token: Address::repeat_byte(5),
            decimals: 6,
        },
        sender: Address::repeat_byte(6),
        recipient: Address::repeat_byte(7),
        refund_account: Address::repeat_byte(6),
        destination_pool: Address::repeat_byte(8),
        reimbursement_account: Address::repeat_byte(9),
        principal,
        fee,
        quote_id: B256::repeat_byte(10),
        destination_expiry_height: 100,
        transfer_nonce: 1,
    }
}

#[test]
fn unpaid_claims_and_refill_notifications_never_create_pool_liquidity() {
    let value = intent(U256::from(10), U256::ZERO);
    let mut pool = PoolAccounting::new(value.asset, U256::ZERO, U256::ZERO).unwrap();
    pool.set_exposure_cap(value.source.zone_id, U256::from(100));

    assert_eq!(
        pool.commit_payment(value.transfer_id(), value.source.zone_id, value.principal),
        Err(AdmissionError::Pool("insufficient spendable liquidity"))
    );
    assert_eq!(pool.funded_balance(), U256::ZERO);

    pool.credit_observed(U256::from(10)).unwrap();
    pool.commit_payment(value.transfer_id(), value.source.zone_id, value.principal)
        .unwrap();
    assert_eq!(pool.funded_balance(), U256::ZERO);
}

#[test]
fn transport_ack_is_not_a_terminal_outcome_or_source_disposition() {
    let mut delivery = DeliveryRecord::queued(1, 1, B256::repeat_byte(1), vec![1]);
    assert_eq!(
        delivery.advance(DeliveryTransition::TransportAcknowledged),
        Ok(true)
    );
    assert!(!delivery.compactable());
    assert_eq!(
        delivery.advance(DeliveryTransition::Queued),
        Err(DeliveryError::Regression)
    );
    delivery
        .advance(DeliveryTransition::TerminalReceived)
        .unwrap();
    assert!(!delivery.compactable());
    delivery
        .advance(DeliveryTransition::SourceDisposed)
        .unwrap();
    assert!(delivery.compactable());
}

#[test]
fn full_intent_binding_and_escrow_arithmetic_fail_before_any_transition() {
    let value = intent(U256::from(10), U256::from(1));
    let mut changed = value.clone();
    changed.reimbursement_account = Address::repeat_byte(11);
    let destination = DestinationRecord::new(&value);
    assert_eq!(
        destination.check_intent(&changed),
        Err(StateError::IntentConflict)
    );

    let overflow = intent(U256::MAX, U256::from(1));
    assert_eq!(
        SourceRecord::locked(&overflow),
        Err(StateError::AmountOverflow)
    );

    let rejected = DestinationDecision::Rejected(RejectionReason::QuoteExpired);
    let mut destination = DestinationRecord::new(&value);
    destination.decide(rejected).unwrap();
    assert_eq!(destination.decide(rejected), Ok(false));
}
