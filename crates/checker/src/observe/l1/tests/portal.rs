//! Portal tests.

use super::*;

#[test]
fn one_receipt_can_require_two_portal_call_families() {
    let tx_hash = B256::repeat_byte(0x10);
    let batch = batch_submitted_log(0, 0);
    let processed = withdrawal_processed_log(0, 1);
    let (imported, receipts) = anchor(vec![receipt(tx_hash, 0, true, vec![batch, processed])]);
    acquisition::authenticate_receipts(&imported, &[tx_hash], &receipts).unwrap();
    let observed = l1_events::ordered_transactions(PORTAL, &[tx_hash], &receipts).unwrap();
    assert_eq!(
        observed[0].required_calls,
        vec![
            PortalCallFamily::SubmitBatch,
            PortalCallFamily::ProcessWithdrawals
        ]
    );
}

#[test]
fn direct_portal_calls_tolerate_unrelated_aa_calls_and_preserve_order() {
    let calldata = submit_batch_calldata();
    let direct = legacy_call(PORTAL, calldata.clone());
    let calls = l1_portal::decode_direct_portal_calls(
        &direct,
        PORTAL,
        0,
        B256::ZERO,
        &[PortalCallFamily::SubmitBatch],
    )
    .unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].as_submit_batch().is_some());

    let wrong_target = legacy_call(EXTERNAL, calldata.clone());
    assert!(matches!(
        l1_portal::decode_direct_portal_calls(
            &wrong_target,
            PORTAL,
            0,
            B256::ZERO,
            &[PortalCallFamily::SubmitBatch],
        ),
        Err(ObservationError::PortalCall(
            PortalCallError::UnsupportedNestedPortalCall {
                target: Some(EXTERNAL),
                ..
            }
        ))
    ));

    let multi = aa_calls(vec![
        Call {
            to: PORTAL.into(),
            value: U256::ZERO,
            input: calldata.clone(),
        },
        Call {
            to: EXTERNAL.into(),
            value: U256::ZERO,
            input: Bytes::new(),
        },
    ]);
    let calls = l1_portal::decode_direct_portal_calls(
        &multi,
        PORTAL,
        0,
        B256::ZERO,
        &[PortalCallFamily::SubmitBatch],
    )
    .unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].as_submit_batch().is_some());

    let ordered = aa_calls(vec![
        Call {
            to: PORTAL.into(),
            value: U256::ZERO,
            input: calldata,
        },
        Call {
            to: PORTAL.into(),
            value: U256::ZERO,
            input: process_withdrawals_calldata(true),
        },
    ]);
    let calls = l1_portal::decode_direct_portal_calls(
        &ordered,
        PORTAL,
        0,
        B256::ZERO,
        &[
            PortalCallFamily::SubmitBatch,
            PortalCallFamily::ProcessWithdrawals,
        ],
    )
    .unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].family(), PortalCallFamily::SubmitBatch);
    assert_eq!(calls[1].family(), PortalCallFamily::ProcessWithdrawals);
}
