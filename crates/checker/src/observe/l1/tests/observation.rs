//! Observation tests.

use super::*;

#[tokio::test]
async fn empty_process_withdrawals_without_events_causes_no_transaction_fetch() {
    let envelope = legacy_call(PORTAL, process_withdrawals_calldata(false));
    let tx_hash = envelope.trie_hash();
    let (imported, receipts) = anchor_with_transactions(
        vec![receipt(tx_hash, 0, true, vec![])],
        std::slice::from_ref(&envelope),
    );
    let block = block_response(&imported, vec![envelope]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_success(&Some(receipts));
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter.clone());

    let observed = observe_l1(&provider, &imported, PORTAL).await.unwrap();
    assert!(observed.protocol_transactions.is_empty());
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn eventful_submit_batch_fetches_once_and_decodes_direct_calldata() {
    let envelope = legacy_call(PORTAL, submit_batch_calldata());
    let tx_hash = envelope.trie_hash();
    let batch_event = batch_submitted_log(0, 500);
    let (imported, receipts) = anchor_with_transactions(
        vec![receipt(tx_hash, 0, true, vec![batch_event])],
        std::slice::from_ref(&envelope),
    );
    let block = block_response(&imported, vec![envelope]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_success(&Some(receipts));
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter.clone());

    let observed = observe_l1(&provider, &imported, PORTAL).await.unwrap();
    assert_eq!(observed.protocol_transactions.len(), 1);
    assert!(
        observed.protocol_transactions[0]
            .direct_calls()
            .first()
            .and_then(DecodedPortalCall::as_submit_batch)
            .is_some()
    );
    assert_eq!(observed.protocol_transactions[0].outcomes.len(), 1);
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn eventful_process_withdrawals_fetches_once_and_retains_input_and_outcome() {
    let envelope = legacy_call(PORTAL, process_withdrawals_calldata(true));
    let tx_hash = envelope.trie_hash();
    let process_event = withdrawal_processed_log(0, 500);
    let (imported, receipts) = anchor_with_transactions(
        vec![receipt(tx_hash, 0, true, vec![process_event])],
        std::slice::from_ref(&envelope),
    );
    let block = block_response(&imported, vec![envelope]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_success(&Some(receipts));
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter.clone());

    let observed = observe_l1(&provider, &imported, PORTAL).await.unwrap();
    let [transaction] = observed.protocol_transactions() else {
        panic!("expected one protocol transaction");
    };
    let call = transaction
        .direct_calls()
        .first()
        .and_then(DecodedPortalCall::as_process_withdrawals)
        .expect("authenticated processWithdrawals input");
    assert_eq!(call.withdrawals.len(), 1);
    assert_eq!(call.remainingQueue, B256::repeat_byte(9));
    let [outcome] = transaction.outcomes() else {
        panic!("expected one ordered outcome");
    };
    assert!(matches!(
        outcome.event(),
        L1ProtocolEvent::Portal(ZonePortal::ZonePortalEvents::WithdrawalProcessed(_))
    ));
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn eventful_submit_batch_rejects_wrong_target_direct_call() {
    let envelope = legacy_call(EXTERNAL, submit_batch_calldata());
    let tx_hash = envelope.trie_hash();
    let batch_event = batch_submitted_log(0, 500);
    let (imported, receipts) = anchor_with_transactions(
        vec![receipt(tx_hash, 0, true, vec![batch_event])],
        std::slice::from_ref(&envelope),
    );
    let block = block_response(&imported, vec![envelope]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_success(&Some(receipts));
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter.clone());

    assert!(matches!(
        observe_l1(&provider, &imported, PORTAL).await,
        Err(ObservationError::PortalCall(PortalCallError::UnsupportedNestedPortalCall {
            transaction_hash,
            target: Some(target),
        })) if transaction_hash == tx_hash && target == EXTERNAL
    ));
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn authenticated_event_and_direct_call_family_must_match() {
    let envelope = legacy_call(PORTAL, submit_batch_calldata());
    let tx_hash = envelope.trie_hash();
    let process_event = withdrawal_processed_log(0, 0);
    let (imported, receipts) = anchor_with_transactions(
        vec![receipt(tx_hash, 0, true, vec![process_event])],
        std::slice::from_ref(&envelope),
    );
    let block = block_response(&imported, vec![envelope]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_success(&Some(receipts));
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);

    assert!(matches!(
        observe_l1(&provider, &imported, PORTAL).await,
        Err(ObservationError::PortalCall(
            PortalCallError::FamilyMismatch {
                expected: PortalCallFamily::ProcessWithdrawals,
                actual: PortalCallFamily::SubmitBatch,
                ..
            }
        ))
    ));
}

#[tokio::test]
async fn eventful_empty_process_withdrawals_fails_closed() {
    let envelope = legacy_call(PORTAL, process_withdrawals_calldata(false));
    let tx_hash = envelope.trie_hash();
    let process_event = withdrawal_processed_log(0, 0);
    let (imported, receipts) = anchor_with_transactions(
        vec![receipt(tx_hash, 0, true, vec![process_event])],
        std::slice::from_ref(&envelope),
    );
    let block = block_response(&imported, vec![envelope]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_success(&Some(receipts));
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);

    assert!(matches!(
        observe_l1(&provider, &imported, PORTAL).await,
        Err(ObservationError::PortalCall(
            PortalCallError::EmptyProcessWithOutcomes { .. }
        ))
    ));
}

#[tokio::test]
async fn eventful_malformed_direct_calldata_fails_closed() {
    let mut malformed = submit_batch_calldata().to_vec();
    malformed.pop();
    let evidence = AuthenticatedDataEvidence::from_bytes(&malformed);
    let envelope = legacy_call(PORTAL, malformed.into());
    let tx_hash = envelope.trie_hash();
    let batch_event = batch_submitted_log(0, 0);
    let (imported, receipts) = anchor_with_transactions(
        vec![receipt(tx_hash, 0, true, vec![batch_event])],
        std::slice::from_ref(&envelope),
    );
    let block = block_response(&imported, vec![envelope]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_success(&Some(receipts));
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);

    assert!(matches!(
        observe_l1(&provider, &imported, PORTAL).await,
        Err(ObservationError::MalformedAuthenticatedData {
            kind: DataSource::SubmitBatchCalldata,
            transaction,
            evidence: actual_evidence,
            ..
        }) if transaction
            == AuthenticatedTransaction::new(ProtocolChain::TempoL1, 0, tx_hash)
            && actual_evidence == evidence
    ));
}

#[tokio::test]
async fn missing_block_receipts_and_incomplete_transaction_blocks_are_source_classified() {
    let (empty_imported, _) = anchor(vec![]);
    let asserter = Asserter::new();
    asserter
        .push_success(&Option::<Block<Transaction<TempoTxEnvelope>, TempoHeaderResponse>>::None);
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);
    assert!(matches!(
        observe_l1(&provider, &empty_imported, PORTAL).await,
        Err(ObservationError::Acquisition(AcquisitionError::Missing {
            kind: AcquisitionSource::L1Block,
            ..
        }))
    ));

    let block = block_response(&empty_imported, vec![]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_success(&Option::<Vec<TempoTransactionReceipt>>::None);
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);
    assert!(matches!(
        observe_l1(&provider, &empty_imported, PORTAL).await,
        Err(ObservationError::Acquisition(AcquisitionError::Missing {
            kind: AcquisitionSource::L1Receipts,
            ..
        }))
    ));

    let envelope = legacy_call(PORTAL, submit_batch_calldata());
    let tx_hash = envelope.trie_hash();
    let (imported, _) = anchor_with_transactions(vec![], std::slice::from_ref(&envelope));
    let mut block = block_response(&imported, vec![envelope]);
    block.transactions = BlockTransactions::Hashes(vec![tx_hash]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);
    assert!(matches!(
        observe_l1(&provider, &imported, PORTAL).await,
        Err(ObservationError::Acquisition(
            AcquisitionError::Inconsistent {
                kind: AcquisitionSource::L1Transaction,
                ..
            }
        ))
    ));
}

#[tokio::test]
async fn transport_failures_are_unavailable_and_source_classified() {
    let (empty_imported, _) = anchor(vec![]);
    let asserter = Asserter::new();
    asserter.push_failure_msg("block transport failure");
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);
    assert_unavailable(
        observe_l1(&provider, &empty_imported, PORTAL)
            .await
            .unwrap_err(),
        AcquisitionSource::L1Block,
    );

    let block = block_response(&empty_imported, vec![]);
    let asserter = Asserter::new();
    asserter.push_success(&Some(block));
    asserter.push_failure_msg("receipt transport failure");
    let provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);
    assert_unavailable(
        observe_l1(&provider, &empty_imported, PORTAL)
            .await
            .unwrap_err(),
        AcquisitionSource::L1Receipts,
    );
}
