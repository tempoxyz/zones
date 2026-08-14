use alloy_consensus::{
    Header, ReceiptWithBloom, Sealable as _, Signed, TxLegacy, transaction::Recovered,
};
use alloy_eips::Encodable2718 as _;
use alloy_network::TransactionResponse as _;
use alloy_primitives::{Address, B256, Bloom, Bytes, Log, LogData, Signature, U256};
use alloy_provider::ProviderBuilder;
use alloy_rpc_types_eth::{
    Block, BlockTransactions, Header as RpcHeader, Log as RpcLog, Transaction, TransactionReceipt,
};
use alloy_sol_types::{SolCall as _, SolEvent as _};
use alloy_transport::mock::Asserter;
use tempo_alloy::{
    TempoNetwork,
    rpc::{TempoHeaderResponse, TempoTransactionReceipt},
};
use tempo_primitives::{
    TempoHeader, TempoReceipt, TempoTxEnvelope, TempoTxType,
    transaction::{Call, TempoSignature, TempoTransaction},
};
use tempo_zone_contracts::ZonePortal;
use zone_precompiles::ecies::AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE;

use super::{acquisition, events as l1_events, observe_l1, portal as l1_portal};
use crate::observe::{
    abi::{DecodedPortalCall, ImportedTempoHeader},
    error::{
        AcquisitionError, AcquisitionSource, AuthenticatedDataEvidence, AuthenticatedTransaction,
        DataSource, ObservationError, PortalCallError, PortalCallFamily, ProtocolChain,
    },
    events::L1ProtocolEvent,
};

const BLOCK_NUMBER: u64 = 42;
const PORTAL: Address = Address::repeat_byte(0x42);
const EXTERNAL: Address = Address::repeat_byte(0xee);

mod exact_header {
    use alloy_primitives::B256;
    use alloy_provider::ProviderBuilder;
    use alloy_rpc_types_eth::{Block, Transaction};
    use alloy_transport::mock::Asserter;
    use tempo_alloy::{TempoNetwork, rpc::TempoHeaderResponse};
    use tempo_primitives::TempoTxEnvelope;

    use super::{anchor, assert_inconsistent, assert_unavailable, block_response};
    use crate::observe::{
        AcquisitionError, AcquisitionSource, ObservationError, acquire_l1_header,
    };

    #[tokio::test]
    async fn acquires_only_a_reported_and_computed_exact_hash() {
        let (imported, _) = anchor(vec![]);
        let requested = imported.hash();

        let asserter = Asserter::new();
        asserter.push_success(&Some(block_response(&imported, Vec::new())));
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone());

        assert_eq!(
            acquire_l1_header(&provider, requested).await.unwrap(),
            imported
        );
        assert!(asserter.read_q().is_empty());

        let mut wrong_reported = block_response(&imported, Vec::new());
        wrong_reported.header.inner.hash = B256::repeat_byte(0xa1);
        let mut wrong_computed = block_response(&imported, Vec::new());
        wrong_computed.header.inner.inner.inner.gas_limit += 1;

        for response in [wrong_reported, wrong_computed] {
            let asserter = Asserter::new();
            asserter.push_success(&Some(response));
            let provider =
                ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);
            assert_inconsistent(
                acquire_l1_header(&provider, requested).await.unwrap_err(),
                AcquisitionSource::L1Block,
            );
        }
    }

    #[tokio::test]
    async fn missing_and_unavailable_exact_headers_remain_acquisition_errors() {
        let requested = B256::repeat_byte(0x42);
        let asserter = Asserter::new();
        asserter.push_success(
            &Option::<Block<Transaction<TempoTxEnvelope>, TempoHeaderResponse>>::None,
        );
        let provider =
            ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);
        assert!(matches!(
            acquire_l1_header(&provider, requested).await,
            Err(ObservationError::Acquisition(AcquisitionError::Missing {
                kind: AcquisitionSource::L1Block,
                ..
            }))
        ));

        let asserter = Asserter::new();
        asserter.push_failure_msg("block transport failure");
        let provider =
            ProviderBuilder::new_with_network::<TempoNetwork>().connect_mocked_client(asserter);
        assert_unavailable(
            acquire_l1_header(&provider, requested).await.unwrap_err(),
            AcquisitionSource::L1Block,
        );
    }
}

fn rpc_log(log: Log, transaction_index: u64, misleading_log_index: u64) -> RpcLog {
    RpcLog {
        inner: log,
        block_hash: Some(B256::repeat_byte(0xfb)),
        block_number: Some(999),
        block_timestamp: None,
        transaction_hash: Some(B256::repeat_byte(0xfa)),
        transaction_index: Some(transaction_index),
        log_index: Some(misleading_log_index),
        removed: false,
    }
}

fn event_log<E: alloy_sol_types::SolEvent>(
    address: Address,
    event: E,
    transaction_index: u64,
    misleading_log_index: u64,
) -> RpcLog {
    rpc_log(
        Log {
            address,
            data: event.encode_log_data(),
        },
        transaction_index,
        misleading_log_index,
    )
}

fn batch_submitted_log(transaction_index: u64, misleading_log_index: u64) -> RpcLog {
    event_log(
        PORTAL,
        ZonePortal::BatchSubmitted {
            withdrawalBatchIndex: 1,
            withdrawalQueueIndex: U256::from(2),
            nextProcessedDepositQueueHash: B256::repeat_byte(3),
            nextBlockHash: B256::repeat_byte(4),
            withdrawalQueueHash: B256::repeat_byte(5),
            lastProcessedDepositNumber: 6,
        },
        transaction_index,
        misleading_log_index,
    )
}

fn withdrawal_processed_log(transaction_index: u64, misleading_log_index: u64) -> RpcLog {
    event_log(
        PORTAL,
        ZonePortal::WithdrawalProcessed {
            to: Address::repeat_byte(1),
            senderTag: B256::repeat_byte(2),
            token: Address::repeat_byte(3),
            amount: 4,
            callbackSuccess: true,
        },
        transaction_index,
        misleading_log_index,
    )
}

fn receipt(
    transaction_hash: B256,
    transaction_index: u64,
    success: bool,
    logs: Vec<RpcLog>,
) -> TempoTransactionReceipt {
    let mut bloom = Bloom::ZERO;
    for log in &logs {
        bloom.accrue_log(&log.inner);
    }
    TempoTransactionReceipt {
        inner: TransactionReceipt {
            inner: ReceiptWithBloom::new(
                TempoReceipt::<RpcLog> {
                    tx_type: TempoTxType::Legacy,
                    success,
                    cumulative_gas_used: 21_000 * (transaction_index + 1),
                    logs,
                },
                bloom,
            ),
            transaction_hash,
            transaction_index: Some(transaction_index),
            block_hash: None,
            block_number: None,
            gas_used: 21_000,
            effective_gas_price: 0,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::repeat_byte(0x11),
            to: Some(PORTAL),
            contract_address: None,
        },
        fee_token: None,
        fee_payer: Address::ZERO,
    }
}

fn consensus_receipts(
    receipts: &[TempoTransactionReceipt],
) -> Vec<ReceiptWithBloom<TempoReceipt<Log>>> {
    receipts
        .iter()
        .map(|receipt| {
            receipt
                .inner
                .inner
                .clone()
                .map_receipt(|receipt| receipt.map_logs(Into::into))
        })
        .collect()
}

fn anchor(
    receipts: Vec<TempoTransactionReceipt>,
) -> (ImportedTempoHeader, Vec<TempoTransactionReceipt>) {
    anchor_with_transactions(receipts, &[])
}

fn anchor_with_transactions(
    mut receipts: Vec<TempoTransactionReceipt>,
    transactions: &[TempoTxEnvelope],
) -> (ImportedTempoHeader, Vec<TempoTransactionReceipt>) {
    let consensus = consensus_receipts(&receipts);
    let receipts_root = alloy_consensus::proofs::calculate_receipt_root(&consensus);
    let transactions_root = alloy_consensus::proofs::calculate_transaction_root(transactions);
    let logs_bloom = consensus
        .iter()
        .fold(Bloom::ZERO, |bloom, receipt| bloom | receipt.bloom_ref());
    let header = TempoHeader {
        inner: Header {
            number: BLOCK_NUMBER,
            transactions_root,
            receipts_root,
            logs_bloom,
            ..Default::default()
        },
        ..Default::default()
    };
    let hash = header.hash_slow();
    for receipt in &mut receipts {
        receipt.inner.block_hash = Some(hash);
        receipt.inner.block_number = Some(BLOCK_NUMBER);
    }
    let imported = ImportedTempoHeader::new(header);
    (imported, receipts)
}

fn block_response(
    imported: &ImportedTempoHeader,
    envelopes: Vec<TempoTxEnvelope>,
) -> Block<Transaction<TempoTxEnvelope>, TempoHeaderResponse> {
    let transactions = envelopes
        .into_iter()
        .enumerate()
        .map(|(index, envelope)| rpc_transaction(envelope, imported, index as u64))
        .collect();
    Block {
        header: TempoHeaderResponse {
            inner: RpcHeader {
                hash: imported.hash(),
                inner: imported.header().clone(),
                total_difficulty: None,
                size: None,
            },
            timestamp_millis: 0,
        },
        uncles: vec![],
        transactions: BlockTransactions::Full(transactions),
        withdrawals: None,
    }
}

fn legacy_call(target: Address, calldata: Bytes) -> TempoTxEnvelope {
    TempoTxEnvelope::Legacy(Signed::new_unhashed(
        TxLegacy {
            to: target.into(),
            input: calldata,
            ..Default::default()
        },
        Signature::test_signature(),
    ))
}

fn aa_calls(calls: Vec<Call>) -> TempoTxEnvelope {
    TempoTxEnvelope::AA(
        TempoTransaction {
            calls,
            ..Default::default()
        }
        .into_signed(TempoSignature::from(Signature::test_signature())),
    )
}

fn rpc_transaction(
    envelope: TempoTxEnvelope,
    imported: &ImportedTempoHeader,
    transaction_index: u64,
) -> Transaction<TempoTxEnvelope> {
    Transaction {
        inner: Recovered::new_unchecked(envelope, Address::repeat_byte(0x11)),
        block_hash: Some(imported.hash()),
        block_number: Some(imported.number()),
        transaction_index: Some(transaction_index),
        effective_gas_price: None,
        block_timestamp: None,
    }
}

fn submit_batch_calldata() -> Bytes {
    ZonePortal::submitBatchCall {
        tempoBlockNumber: 1,
        recentTempoBlockNumber: 2,
        blockTransition: ZonePortal::BlockTransition {
            prevBlockHash: B256::repeat_byte(3),
            nextBlockHash: B256::repeat_byte(4),
        },
        depositQueueTransition: ZonePortal::DepositQueueTransition {
            prevProcessedHash: B256::repeat_byte(5),
            nextProcessedHash: B256::repeat_byte(6),
            prevDepositNumber: 7,
            nextDepositNumber: 8,
        },
        withdrawalQueueHash: B256::repeat_byte(9),
        verifierConfig: Bytes::from_static(b"config"),
        proof: Bytes::from_static(b"proof"),
        nextZoneHeight: U256::from(10),
        signatures: vec![Bytes::from_static(b"signature")],
    }
    .abi_encode()
    .into()
}

fn process_withdrawals_calldata(nonempty: bool) -> Bytes {
    let withdrawals = nonempty
        .then(|| ZonePortal::Withdrawal {
            token: Address::repeat_byte(1),
            senderTag: B256::repeat_byte(2),
            to: Address::repeat_byte(3),
            amount: 4,
            memo: B256::repeat_byte(5),
            gasLimit: 6,
            fallbackNonce: 7,
            callbackData: Bytes::new(),
            encryptedSender: Bytes::from(vec![8; AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE]),
        })
        .into_iter()
        .collect();
    ZonePortal::processWithdrawalsCall {
        withdrawals,
        remainingQueue: B256::repeat_byte(9),
    }
    .abi_encode()
    .into()
}

fn assert_inconsistent(error: ObservationError, source: AcquisitionSource) {
    assert!(matches!(
        error,
        ObservationError::Acquisition(AcquisitionError::Inconsistent { kind, .. }) if kind == source
    ));
}

fn assert_unavailable(error: ObservationError, source: AcquisitionSource) {
    assert!(matches!(
        error,
        ObservationError::Acquisition(AcquisitionError::Unavailable { kind, .. }) if kind == source
    ));
}

mod authentication;
mod events;
mod portal;

mod observation;
