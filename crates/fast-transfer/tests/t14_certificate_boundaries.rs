use alloy_primitives::{Address, B256, U256};
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use zone_fast_transfer::{CertificateError, EpochRoster, QuorumVerifier};
use zone_primitives::fast_transfer::{
    AssetId, CanonicalEncode, CertificateBody, CodecError, MAX_CERTIFICATE_BYTES,
    OutcomeCertificate, PeerMessage, SettlementStatement, SignatureBytes, TransferIntent,
    TransferOutcome, ZoneDomain,
};

fn fixture() -> (TransferIntent, QuorumVerifier, [PrivateKeySigner; 3]) {
    let signers = [
        PrivateKeySigner::from_bytes(&B256::with_last_byte(0x31)).unwrap(),
        PrivateKeySigner::from_bytes(&B256::with_last_byte(0x32)).unwrap(),
        PrivateKeySigner::from_bytes(&B256::with_last_byte(0x33)).unwrap(),
    ];
    let members = signers.each_ref().map(|signer| signer.address());
    let mut destination = ZoneDomain {
        l1_chain_id: 42,
        zone_id: 2,
        chain_id: 4_002,
        portal: Address::repeat_byte(0x22),
        authority_epoch: 7,
        roster_hash: B256::ZERO,
        protocol_version: 1,
    };
    destination.roster_hash = EpochRoster::calculate_hash(destination, members);
    let source = ZoneDomain {
        zone_id: 1,
        chain_id: 4_001,
        portal: Address::repeat_byte(0x11),
        roster_hash: B256::repeat_byte(0x11),
        ..destination
    };
    let intent = TransferIntent {
        source,
        destination,
        asset: AssetId {
            l1_token: Address::repeat_byte(0x44),
            source_token: Address::repeat_byte(0x45),
            destination_token: Address::repeat_byte(0x46),
            decimals: 6,
        },
        sender: Address::repeat_byte(0x51),
        recipient: Address::repeat_byte(0x52),
        refund_account: Address::repeat_byte(0x51),
        destination_pool: Address::repeat_byte(0x53),
        reimbursement_account: Address::repeat_byte(0x54),
        principal: U256::from(10_000_000),
        fee: U256::from(123),
        quote_id: B256::repeat_byte(0x55),
        destination_expiry_height: 900,
        transfer_nonce: 19,
    };
    let roster = EpochRoster::new(destination, members).unwrap();
    (intent, QuorumVerifier::new(roster), signers)
}

fn unsigned_paid(intent: &TransferIntent) -> OutcomeCertificate {
    OutcomeCertificate {
        body: CertificateBody {
            transfer_id: intent.transfer_id(),
            intent_hash: intent.intent_hash(),
            zone: intent.destination,
            log_term: 3,
            log_index: 8,
            block_height: 12,
            block_hash: B256::repeat_byte(0x61),
            state_root: B256::repeat_byte(0x62),
            transaction_hash: B256::repeat_byte(0x63),
            outcome: TransferOutcome::Paid {
                pool: intent.destination_pool,
                recipient: intent.recipient,
                principal: intent.principal,
            },
        },
        signatures: [SignatureBytes([0; 65]); 2],
    }
}

fn sign_outcome(
    verifier: &QuorumVerifier,
    signers: &[PrivateKeySigner; 3],
    mut certificate: OutcomeCertificate,
) -> OutcomeCertificate {
    let digest = verifier.outcome_digest(&certificate);
    certificate.signatures = [0, 1]
        .map(|index| SignatureBytes(signers[index].sign_hash_sync(&digest).unwrap().as_bytes()));
    certificate
}

#[test]
fn committed_paid_certificate_requires_two_distinct_epoch_members() {
    let (intent, verifier, signers) = fixture();
    let mut certificate = sign_outcome(&verifier, &signers, unsigned_paid(&intent));

    let recovered = verifier.verify_outcome(&certificate, &intent).unwrap();
    assert_eq!(recovered, [signers[0].address(), signers[1].address()]);

    let digest = verifier.outcome_digest(&certificate);
    certificate.signatures = [
        SignatureBytes(signers[0].sign_hash_sync(&digest).unwrap().as_bytes()),
        SignatureBytes(signers[0].sign_hash_sync(&digest).unwrap().as_bytes()),
    ];
    assert_eq!(
        verifier.verify_outcome(&certificate, &intent),
        Err(CertificateError::DuplicateSigner)
    );
}

#[test]
fn every_token_relevant_mutation_has_zero_authorized_effect() {
    let (intent, verifier, signers) = fixture();
    let certificate = sign_outcome(&verifier, &signers, unsigned_paid(&intent));
    let literal_balances = (U256::from(100_000_000), U256::ZERO);

    let mutations: Vec<CertificateMutation> = vec![
        Box::new(|value| value.body.transfer_id = B256::repeat_byte(1)),
        Box::new(|value| value.body.intent_hash = B256::repeat_byte(2)),
        Box::new(|value| value.body.zone.authority_epoch += 1),
        Box::new(|value| value.body.zone.roster_hash = B256::repeat_byte(3)),
        Box::new(|value| value.body.zone.l1_chain_id += 1),
        Box::new(|value| value.body.zone.zone_id += 1),
        Box::new(|value| value.body.zone.chain_id += 1),
        Box::new(|value| value.body.zone.portal = Address::repeat_byte(9)),
        Box::new(|value| value.body.zone.protocol_version += 1),
        Box::new(|value| value.body.log_term += 1),
        Box::new(|value| value.body.log_index += 1),
        Box::new(|value| value.body.block_height += 1),
        Box::new(|value| value.body.block_hash = B256::repeat_byte(4)),
        Box::new(|value| value.body.state_root = B256::repeat_byte(5)),
        Box::new(|value| value.body.transaction_hash = B256::repeat_byte(6)),
        Box::new(|value| {
            if let TransferOutcome::Paid { pool, .. } = &mut value.body.outcome {
                *pool = Address::repeat_byte(7);
            }
        }),
        Box::new(|value| {
            if let TransferOutcome::Paid { recipient, .. } = &mut value.body.outcome {
                *recipient = Address::repeat_byte(8);
            }
        }),
        Box::new(|value| {
            if let TransferOutcome::Paid { principal, .. } = &mut value.body.outcome {
                *principal += U256::from(1);
            }
        }),
    ];

    for mutate in mutations {
        let mut changed = certificate.clone();
        mutate(&mut changed);
        assert!(verifier.verify_outcome(&changed, &intent).is_err());
        assert_eq!(literal_balances, (U256::from(100_000_000), U256::ZERO));
    }
}

#[test]
fn intent_body_nonce_and_signature_domains_cannot_be_reused() {
    let (intent, verifier, signers) = fixture();
    let certificate = sign_outcome(&verifier, &signers, unsigned_paid(&intent));

    let mut substituted = intent.clone();
    substituted.recipient = Address::repeat_byte(0xaa);
    assert_eq!(substituted.transfer_id(), intent.transfer_id());
    assert_ne!(substituted.intent_hash(), intent.intent_hash());
    assert_eq!(
        verifier.verify_outcome(&certificate, &substituted),
        Err(CertificateError::IntentMismatch)
    );

    substituted = intent.clone();
    substituted.transfer_nonce += 1;
    assert_ne!(substituted.transfer_id(), intent.transfer_id());
    assert_eq!(
        verifier.verify_outcome(&certificate, &substituted),
        Err(CertificateError::IntentMismatch)
    );

    let statement = SettlementStatement {
        zone: intent.destination,
        settled_height: certificate.body.block_height,
        settled_block_hash: certificate.body.block_hash,
        transition_hash: certificate.body.state_root,
        anchor_hash: B256::repeat_byte(0xbb),
    };
    let settlement_digest = verifier.settlement_digest(&statement);
    let mut wrong_domain = certificate;
    wrong_domain.signatures = [0, 1].map(|index| {
        SignatureBytes(
            signers[index]
                .sign_hash_sync(&settlement_digest)
                .unwrap()
                .as_bytes(),
        )
    });
    assert!(verifier.verify_outcome(&wrong_domain, &intent).is_err());
}

#[test]
fn every_intent_asset_account_amount_and_epoch_field_is_bound() {
    let (intent, verifier, signers) = fixture();
    let certificate = sign_outcome(&verifier, &signers, unsigned_paid(&intent));
    let mutations: Vec<IntentMutation> = vec![
        Box::new(|value| value.source.l1_chain_id += 1),
        Box::new(|value| value.source.zone_id += 1),
        Box::new(|value| value.source.chain_id += 1),
        Box::new(|value| value.source.portal = Address::repeat_byte(1)),
        Box::new(|value| value.source.authority_epoch += 1),
        Box::new(|value| value.source.roster_hash = B256::repeat_byte(2)),
        Box::new(|value| value.source.protocol_version += 1),
        Box::new(|value| value.destination.authority_epoch += 1),
        Box::new(|value| value.asset.l1_token = Address::repeat_byte(3)),
        Box::new(|value| value.asset.source_token = Address::repeat_byte(4)),
        Box::new(|value| value.asset.destination_token = Address::repeat_byte(5)),
        Box::new(|value| value.asset.decimals += 1),
        Box::new(|value| value.sender = Address::repeat_byte(6)),
        Box::new(|value| value.recipient = Address::repeat_byte(7)),
        Box::new(|value| value.refund_account = Address::repeat_byte(8)),
        Box::new(|value| value.destination_pool = Address::repeat_byte(9)),
        Box::new(|value| value.reimbursement_account = Address::repeat_byte(10)),
        Box::new(|value| value.principal += U256::from(1)),
        Box::new(|value| value.fee += U256::from(1)),
        Box::new(|value| value.quote_id = B256::repeat_byte(11)),
        Box::new(|value| value.destination_expiry_height += 1),
        Box::new(|value| value.transfer_nonce += 1),
    ];

    for mutate in mutations {
        let mut changed = intent.clone();
        mutate(&mut changed);
        assert!(verifier.verify_outcome(&certificate, &changed).is_err());
    }
}

#[test]
fn literal_cross_language_intent_and_transport_vectors_are_stable() {
    let (mut intent, _, _) = fixture();
    intent.destination.roster_hash = B256::ZERO;
    let expected_intent = "000000000000002a000000010000000000000fa11111111111111111111111111111111111111111000000000000000711111111111111111111111111111111111111111111111111111111111111110001000000000000002a000000020000000000000fa2222222222222222222222222222222222222222200000000000000070000000000000000000000000000000000000000000000000000000000000000000144444444444444444444444444444444444444444545454545454545454545454545454545454545464646464646464646464646464646464646464606515151515151515151515151515151515151515152525252525252525252525252525252525252525151515151515151515151515151515151515151535353535353535353535353535353535353535354545454545454545454545454545454545454540000000000000000000000000000000000000000000000000000000000989680000000000000000000000000000000000000000000000000000000000000007b555555555555555555555555555555555555555555555555555555555555555500000000000003840000000000000013";
    assert_eq!(
        alloy_primitives::hex::encode(intent.canonical_bytes()),
        expected_intent
    );
    assert_eq!(
        alloy_primitives::hex::encode(intent.intent_hash()),
        "9ef4df225c1d1d6487dc52e9129b3867d657acce9803fc02fc0be499a64c4a0d"
    );
    assert_eq!(
        alloy_primitives::hex::encode(intent.transfer_id()),
        "039eb272e2e8cf94af54779c37c9f71447b8aa4c64999875ba376aad07f55a8c"
    );
    assert_eq!(
        alloy_primitives::hex::encode(
            PeerMessage::Acknowledgment {
                stream: 4,
                sequence: 9,
            }
            .canonical_bytes()
        ),
        "0200000000000000040000000000000009"
    );
}

#[test]
fn canonical_certificate_boundary_rejects_trailing_and_oversized_bytes() {
    let (intent, verifier, signers) = fixture();
    let certificate = sign_outcome(&verifier, &signers, unsigned_paid(&intent));
    let canonical = certificate.canonical_bytes();
    assert_eq!(OutcomeCertificate::decode(&canonical).unwrap(), certificate);

    let mut trailing = canonical;
    trailing.push(0);
    assert_eq!(
        OutcomeCertificate::decode(&trailing),
        Err(CodecError::TrailingBytes)
    );
    assert!(matches!(
        OutcomeCertificate::decode(&vec![0; MAX_CERTIFICATE_BYTES + 1]),
        Err(CodecError::TooLarge { .. })
    ));
}

type CertificateMutation = Box<dyn Fn(&mut OutcomeCertificate)>;
type IntentMutation = Box<dyn Fn(&mut TransferIntent)>;
