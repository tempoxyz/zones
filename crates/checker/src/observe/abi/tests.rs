use alloy_consensus::Header;
use alloy_primitives::{Address, U256};

use super::*;
use crate::observe::ProtocolChain;

fn l2_transaction(index: usize) -> AuthenticatedTransaction {
    AuthenticatedTransaction::new(ProtocolChain::ZoneL2, index, B256::repeat_byte(0xa1))
}

fn l1_transaction(index: usize) -> AuthenticatedTransaction {
    AuthenticatedTransaction::new(ProtocolChain::TempoL1, index, B256::repeat_byte(0xb1))
}

fn decode_advance(calldata: &[u8]) -> Result<DecodedAdvanceTempo, ObservationError> {
    decode_advance_tempo(calldata, l2_transaction(0))
}

fn decode_finalize(calldata: &[u8]) -> Result<DecodedFinalization, ObservationError> {
    decode_finalization(calldata, l2_transaction(1))
}

fn decode_portal(calldata: &[u8]) -> Result<DecodedPortalCall, ObservationError> {
    decode_portal_call(calldata, l1_transaction(2))
}

fn header_bytes(number: u64) -> Bytes {
    let header = TempoHeader {
        inner: Header {
            number,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut encoded = Vec::new();
    header.encode(&mut encoded);
    encoded.into()
}

fn single_header_advance_call() -> Vec<u8> {
    IZoneInbox::advanceTempoCall {
        header: header_bytes(7),
        deposits: Vec::new(),
        decryptions: Vec::new(),
        enabledTokens: Vec::new(),
    }
    .abi_encode()
}

fn ordinary_deposit() -> ZonePortal::Deposit {
    ZonePortal::Deposit {
        token: Address::repeat_byte(1),
        sender: Address::repeat_byte(2),
        amount: 3,
        tempoRefundRecipient: Address::repeat_byte(4),
        keyIndex: U256::from(5),
        encrypted: ZonePortal::DepositPayload {
            ephemeralPubkeyX: B256::repeat_byte(6),
            ephemeralPubkeyYParity: 2,
            ciphertext: Bytes::from(vec![7; ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE]),
            nonce: [8; 12].into(),
            tag: [9; 16].into(),
        },
    }
}

fn advance_with_ordinary_deposit_data(deposit_data: Vec<u8>) -> Vec<u8> {
    IZoneInbox::advanceTempoCall {
        header: header_bytes(7),
        deposits: vec![IZoneInbox::QueuedDeposit {
            depositType: IZoneInbox::DepositType::Deposit,
            depositData: deposit_data.into(),
        }],
        decryptions: vec![IZoneInbox::DecryptionData {
            sharedSecret: B256::ZERO,
            sharedSecretYParity: 0,
            cpProof: IZoneInbox::ChaumPedersenProof {
                s: B256::ZERO,
                c: B256::ZERO,
            },
        }],
        enabledTokens: Vec::new(),
    }
    .abi_encode()
}

fn submit_batch_call(signatures: Vec<Bytes>) -> Vec<u8> {
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
        signatures,
    }
    .abi_encode()
}

fn usize_word(data: &[u8], offset: usize) -> usize {
    U256::from_be_slice(&data[offset..offset + WORD]).to::<usize>()
}

fn set_usize_word(data: &mut [u8], offset: usize, value: usize) {
    data[offset..offset + WORD].copy_from_slice(&U256::from(value).to_be_bytes::<WORD>());
}

fn assert_malformed<T: core::fmt::Debug>(
    case: &str,
    result: Result<T, ObservationError>,
    expected: DataSource,
) {
    match result {
        Err(ObservationError::MalformedAuthenticatedData {
            kind, transaction, ..
        }) => {
            assert_eq!(kind, expected);
            let expected_transaction = match expected {
                DataSource::FinalizationCalldata => l2_transaction(1),
                DataSource::AdvanceTempoCalldata
                | DataSource::AdvanceHeaderRlp
                | DataSource::OrdinaryDepositData
                | DataSource::WithdrawalBounceBackData => l2_transaction(0),
                DataSource::ProcessWithdrawalsCalldata
                | DataSource::SubmitBatchCalldata
                | DataSource::PortalTransactionCalldata => l1_transaction(2),
            };
            assert_eq!(transaction, expected_transaction);
        }
        other => panic!("{case}: expected malformed {expected}, got {other:?}"),
    }
}

#[test]
fn selectors_are_pinned() {
    assert_eq!(
        IZoneInbox::advanceTempoCall::SELECTOR.as_slice(),
        &[0x97, 0xca, 0xc0, 0xfb]
    );
    assert_eq!(
        &single_header_advance_call()[..SELECTOR_LEN],
        IZoneInbox::advanceTempoCall::SELECTOR.as_slice()
    );
}

#[test]
fn advance_decodes_canonical_payload() {
    let ordinary = ordinary_deposit();
    let bounce_back = IZoneInbox::WithdrawalBounceBackDeposit {
        token: Address::repeat_byte(10),
        to: Address::repeat_byte(11),
        amount: 12,
    };
    let calldata = IZoneInbox::advanceTempoCall {
        header: header_bytes(7),
        deposits: vec![
            IZoneInbox::QueuedDeposit {
                depositType: IZoneInbox::DepositType::Deposit,
                depositData: ordinary.abi_encode().into(),
            },
            IZoneInbox::QueuedDeposit {
                depositType: IZoneInbox::DepositType::WithdrawalBounceBack,
                depositData: bounce_back.abi_encode().into(),
            },
        ],
        decryptions: vec![IZoneInbox::DecryptionData {
            sharedSecret: B256::repeat_byte(13),
            sharedSecretYParity: 2,
            cpProof: IZoneInbox::ChaumPedersenProof {
                s: B256::repeat_byte(14),
                c: B256::repeat_byte(15),
            },
        }],
        enabledTokens: vec![IZoneInbox::EnabledToken {
            token: Address::repeat_byte(16),
            name: "Token".into(),
            symbol: "TKN".into(),
            currency: "USD".into(),
        }],
    }
    .abi_encode();

    let decoded = decode_advance(&calldata).unwrap();
    assert_eq!(decoded.imported_header().number(), 7);
    assert!(decoded.deposits[0].as_ordinary().is_some());
    assert!(decoded.deposits[1].as_withdrawal_bounce_back().is_some());
    assert_eq!(decoded.enabled_tokens.len(), 1);
}

#[test]
fn deposit_preflight_rejects_invalid_layout() {
    let mut oversized = ordinary_deposit().abi_encode();
    assert_eq!(oversized.len(), ORDINARY_DEPOSIT_ENCODED_SIZE);
    oversized.extend([0; WORD]);
    let expected_evidence = AuthenticatedDataEvidence::from_bytes(&oversized);
    let calldata = advance_with_ordinary_deposit_data(oversized);

    let Err(ObservationError::MalformedAuthenticatedData {
        kind,
        transaction,
        evidence,
        ..
    }) = decode_advance(&calldata)
    else {
        panic!("expected malformed ordinary deposit data");
    };
    assert_eq!(kind, DataSource::OrdinaryDepositData);
    assert_eq!(transaction, l2_transaction(0));
    assert_eq!(evidence, expected_evidence);
    assert_ne!(
        evidence,
        AuthenticatedDataEvidence::from_bytes(&calldata),
        "nested evidence must hash depositData, not the outer advanceTempo calldata"
    );

    let canonical = ordinary_deposit().abi_encode();
    let deposit = usize_word(&canonical, 0);
    let encrypted = deposit + usize_word(&canonical, deposit + 5 * WORD);
    let ciphertext = encrypted + usize_word(&canonical, encrypted + 2 * WORD);

    for (name, offset, value) in [
        ("unaligned deposit offset", 0, WORD + 1),
        ("backward encrypted offset", deposit + 5 * WORD, 5 * WORD),
        ("backward ciphertext offset", encrypted + 2 * WORD, 4 * WORD),
        (
            "oversized ciphertext",
            ciphertext,
            ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE + 1,
        ),
    ] {
        let mut malformed = canonical.clone();
        set_usize_word(&mut malformed, offset, value);
        assert_malformed(
            name,
            decode_advance(&advance_with_ordinary_deposit_data(malformed)),
            DataSource::OrdinaryDepositData,
        );
    }
}

#[test]
fn advance_rejects_noncanonical_encoding() {
    let mut trailing_abi = single_header_advance_call();
    trailing_abi.extend([0_u8; WORD]);

    let mut header = header_bytes(7).to_vec();
    let mut cursor = header.as_slice();
    let envelope = alloy_rlp::Header::decode(&mut cursor).unwrap();
    assert!(envelope.list);
    let first_item = header.len() - cursor.len();
    assert_eq!(header[first_item], 0x80, "first Tempo header field is zero");
    header[first_item] = 0x00;
    let noncanonical_rlp = IZoneInbox::advanceTempoCall {
        header: header.into(),
        deposits: Vec::new(),
        decryptions: Vec::new(),
        enabledTokens: Vec::new(),
    }
    .abi_encode();

    let mut header = header_bytes(7).to_vec();
    header.push(0x80);
    let trailing_rlp = IZoneInbox::advanceTempoCall {
        header: header.into(),
        deposits: Vec::new(),
        decryptions: Vec::new(),
        enabledTokens: Vec::new(),
    }
    .abi_encode();

    for (name, calldata, source) in [
        (
            "trailing ABI word",
            trailing_abi,
            DataSource::AdvanceTempoCalldata,
        ),
        (
            "noncanonical header RLP",
            noncanonical_rlp,
            DataSource::AdvanceHeaderRlp,
        ),
        (
            "trailing header RLP",
            trailing_rlp,
            DataSource::AdvanceHeaderRlp,
        ),
    ] {
        assert_malformed(name, decode_advance(&calldata), source);
    }
}

#[test]
fn dynamic_arrays_enforce_bounds() {
    for (name, head_word, maximum) in [
        ("deposit cap", 1, MAX_DEPOSITS_PER_TEMPO_BLOCK),
        ("enabled-token cap", 3, MAX_TOKENS_ENABLED_PER_TEMPO_BLOCK),
    ] {
        let mut calldata = single_header_advance_call();
        let payload = &mut calldata[SELECTOR_LEN..];
        let array = usize_word(payload, head_word * WORD);
        set_usize_word(payload, array, maximum + 1);
        assert_malformed(
            name,
            decode_advance(&calldata),
            DataSource::AdvanceTempoCalldata,
        );
    }

    let mut calldata = submit_batch_call(Vec::new());
    let payload = &mut calldata[SELECTOR_LEN..];
    let signatures = usize_word(payload, 12 * WORD);
    set_usize_word(payload, signatures, MAX_SEQUENCERS + 1);
    assert_malformed(
        "signature cap",
        decode_portal(&calldata),
        DataSource::SubmitBatchCalldata,
    );

    for (name, bad_offset) in [
        ("unaligned header offset", 4 * WORD + 1),
        (
            "header offset at end",
            single_header_advance_call().len() - SELECTOR_LEN,
        ),
    ] {
        let mut calldata = single_header_advance_call();
        set_usize_word(&mut calldata[SELECTOR_LEN..], 0, bad_offset);
        assert_malformed(
            name,
            decode_advance(&calldata),
            DataSource::AdvanceTempoCalldata,
        );
    }

    let mut calldata = single_header_advance_call();
    let payload = &mut calldata[SELECTOR_LEN..];
    let header = usize_word(payload, 0);
    set_usize_word(payload, header, payload.len());
    assert_malformed(
        "header length exceeds payload",
        decode_advance(&calldata),
        DataSource::AdvanceTempoCalldata,
    );

    let mut calldata = submit_batch_call(vec![Bytes::from_static(b"signature")]);
    let payload = &mut calldata[SELECTOR_LEN..];
    let signatures = usize_word(payload, 12 * WORD);
    let signature_table = signatures + WORD;
    set_usize_word(payload, signature_table, payload.len());
    assert_malformed(
        "signature element offset exceeds payload",
        decode_portal(&calldata),
        DataSource::SubmitBatchCalldata,
    );
}

#[test]
fn finalization_enforces_shape() {
    let call = IZoneOutbox::finalizeWithdrawalBatchCall {
        count: U256::from(2),
        blockNumber: 9,
        encryptedSenders: vec![
            Bytes::new(),
            Bytes::from(vec![7; AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE]),
        ],
    }
    .abi_encode();
    let decoded = decode_finalize(&call).unwrap();
    assert_eq!(decoded.count, 2);
    assert_eq!(decoded.block_number, 9);
    assert_eq!(decoded.encrypted_senders[0].len(), 0);
    assert_eq!(
        decoded.encrypted_senders[1].len(),
        AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE
    );

    let count_mismatch = IZoneOutbox::finalizeWithdrawalBatchCall {
        count: U256::from(2),
        blockNumber: 9,
        encryptedSenders: vec![Bytes::new()],
    }
    .abi_encode();
    let malformed_length = IZoneOutbox::finalizeWithdrawalBatchCall {
        count: U256::from(1),
        blockNumber: 9,
        encryptedSenders: vec![Bytes::from(vec![0; 1])],
    }
    .abi_encode();

    for (name, calldata) in [
        ("count mismatch", count_mismatch),
        ("invalid encrypted-sender length", malformed_length),
    ] {
        assert_malformed(
            name,
            decode_finalize(&calldata),
            DataSource::FinalizationCalldata,
        );
    }
}

#[test]
fn process_withdrawals_enforces_shape_and_callback_bound() {
    let valid = ZonePortal::processWithdrawalsCall {
        withdrawals: vec![ZonePortal::Withdrawal {
            token: Address::repeat_byte(1),
            senderTag: B256::repeat_byte(2),
            to: Address::repeat_byte(3),
            amount: 4,
            memo: B256::repeat_byte(5),
            gasLimit: 6,
            fallbackNonce: 7,
            callbackData: Bytes::from_static(b"callback"),
            encryptedSender: Bytes::from(vec![8; AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE]),
        }],
        remainingQueue: B256::repeat_byte(9),
    }
    .abi_encode();
    assert!(
        decode_portal(&valid)
            .unwrap()
            .is_nonempty_process_withdrawals()
    );

    let oversized_callback = ZonePortal::processWithdrawalsCall {
        withdrawals: vec![ZonePortal::Withdrawal {
            token: Address::ZERO,
            senderTag: B256::ZERO,
            to: Address::ZERO,
            amount: 1,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackNonce: 0,
            callbackData: Bytes::from(vec![0; MAX_CALLBACK_DATA_SIZE + 1]),
            encryptedSender: Bytes::new(),
        }],
        remainingQueue: B256::ZERO,
    }
    .abi_encode();
    assert_malformed(
        "oversized callback",
        decode_portal(&oversized_callback),
        DataSource::ProcessWithdrawalsCalldata,
    );
}

#[test]
fn submit_batch_decodes_canonical_payload() {
    let call = submit_batch_call(vec![Bytes::from_static(b"signature")]);
    assert!(decode_portal(&call).unwrap().as_submit_batch().is_some());
}

#[test]
fn portal_calls_accept_solidity_trailing_bytes() {
    let mut batch = submit_batch_call(vec![Bytes::from_static(b"signature")]);
    batch.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert!(decode_portal(&batch).unwrap().as_submit_batch().is_some());

    let mut withdrawals = ZonePortal::processWithdrawalsCall {
        withdrawals: Vec::new(),
        remainingQueue: B256::ZERO,
    }
    .abi_encode();
    withdrawals.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert!(
        decode_portal(&withdrawals)
            .unwrap()
            .as_process_withdrawals()
            .is_some()
    );
}
