//! Events tests.

use super::*;

#[test]
fn authenticated_event_order_uses_receipt_vectors_not_rpc_log_metadata() {
    let hashes = [B256::repeat_byte(0x10), B256::repeat_byte(0x20)];
    let external = rpc_log(
        Log {
            address: EXTERNAL,
            data: LogData::new_unchecked(vec![B256::repeat_byte(0xff)], Bytes::new()),
        },
        91,
        900,
    );
    let config = event_log(
        PORTAL,
        ZonePortal::BouncebackGasUpdated { bouncebackGas: 7 },
        92,
        800,
    );
    let ignored = event_log(
        PORTAL,
        ZonePortal::LeaderUpdated {
            previousLeader: Address::repeat_byte(1),
            newLeader: Address::repeat_byte(2),
            epoch: 3,
            activationTempoBlock: 4,
        },
        93,
        700,
    );
    let operation = batch_submitted_log(94, 600);
    let (imported, receipts) = anchor(vec![
        receipt(hashes[0], 0, true, vec![external, config, ignored]),
        receipt(hashes[1], 1, true, vec![operation]),
    ]);
    acquisition::authenticate_receipts(&imported, &hashes, &receipts).unwrap();

    let observed = l1_events::ordered_transactions(PORTAL, &hashes, &receipts).unwrap();
    assert_eq!(observed.len(), 2);
    assert!(matches!(
        observed[0].outcomes[0].event,
        L1ProtocolEvent::Portal(ZonePortal::ZonePortalEvents::BouncebackGasUpdated(_))
    ));
    assert_eq!(
        observed[1].required_calls,
        vec![PortalCallFamily::SubmitBatch]
    );
}

#[test]
fn authenticated_event_order_preserves_operation_before_config() {
    let hashes = [B256::repeat_byte(0x10), B256::repeat_byte(0x20)];
    let operation = batch_submitted_log(94, 600);
    let external = rpc_log(
        Log {
            address: EXTERNAL,
            data: LogData::new_unchecked(vec![B256::repeat_byte(0xff)], Bytes::new()),
        },
        91,
        900,
    );
    let config = event_log(
        PORTAL,
        ZonePortal::BouncebackGasUpdated { bouncebackGas: 7 },
        92,
        800,
    );
    let ignored = event_log(
        PORTAL,
        ZonePortal::LeaderUpdated {
            previousLeader: Address::repeat_byte(1),
            newLeader: Address::repeat_byte(2),
            epoch: 3,
            activationTempoBlock: 4,
        },
        93,
        700,
    );
    let (imported, receipts) = anchor(vec![
        receipt(hashes[0], 0, true, vec![operation]),
        receipt(hashes[1], 1, true, vec![external, config, ignored]),
    ]);
    acquisition::authenticate_receipts(&imported, &hashes, &receipts).unwrap();

    let observed = l1_events::ordered_transactions(PORTAL, &hashes, &receipts).unwrap();
    assert_eq!(observed.len(), 2);

    let operation = &observed[0].outcomes[0];
    assert!(matches!(
        operation.event,
        L1ProtocolEvent::Portal(ZonePortal::ZonePortalEvents::BatchSubmitted(_))
    ));
    assert_eq!(
        observed[0].required_calls,
        vec![PortalCallFamily::SubmitBatch]
    );

    let config = &observed[1].outcomes[0];
    assert!(matches!(
        config.event,
        L1ProtocolEvent::Portal(ZonePortal::ZonePortalEvents::BouncebackGasUpdated(_))
    ));
    assert!(observed[1].required_calls.is_empty());
}

#[test]
fn malformed_and_unknown_configured_portal_logs_fail_closed_but_external_logs_do_not() {
    let tx_hash = B256::repeat_byte(0x10);
    let external = rpc_log(
        Log {
            address: EXTERNAL,
            data: LogData::new_unchecked(vec![B256::repeat_byte(0x77)], Bytes::new()),
        },
        0,
        0,
    );
    let (imported, receipts) = anchor(vec![receipt(tx_hash, 0, true, vec![external])]);
    acquisition::authenticate_receipts(&imported, &[tx_hash], &receipts).unwrap();
    assert!(
        l1_events::ordered_transactions(PORTAL, &[tx_hash], &receipts)
            .unwrap()
            .is_empty()
    );

    for log in [
        rpc_log(
            Log {
                address: PORTAL,
                data: LogData::new_unchecked(vec![B256::repeat_byte(0x66)], Bytes::new()),
            },
            0,
            0,
        ),
        rpc_log(
            Log {
                address: PORTAL,
                data: LogData::new_unchecked(
                    vec![ZonePortal::BouncebackGasUpdated::SIGNATURE_HASH],
                    Bytes::from_static(b"malformed"),
                ),
            },
            0,
            0,
        ),
    ] {
        let (imported, receipts) = anchor(vec![receipt(tx_hash, 0, true, vec![log])]);
        acquisition::authenticate_receipts(&imported, &[tx_hash], &receipts).unwrap();
        assert!(matches!(
            l1_events::ordered_transactions(PORTAL, &[tx_hash], &receipts),
            Err(ObservationError::ProtocolEvent { .. })
        ));
    }
}
