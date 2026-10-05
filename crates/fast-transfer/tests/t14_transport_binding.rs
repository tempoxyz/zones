//! Certificate keys must authenticate the actual encrypted peers and exact delivery position.

use alloy_primitives::{Address, B256};
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use zone_fast_transfer::{AuthenticatedPeerSession, EpochRoster, TransportError};
use zone_primitives::fast_transfer::{
    CanonicalEncode as _, SignatureBytes, TransportSessionProof, ZoneDomain,
};

fn fixture() -> (
    [EpochRoster; 2],
    [PrivateKeySigner; 2],
    [TransportSessionProof; 2],
) {
    // Deterministic public test keys; never production signers.
    let signers =
        [0x31, 0x41].map(|key| PrivateKeySigner::from_bytes(&B256::with_last_byte(key)).unwrap());
    let rosters = std::array::from_fn(|index| {
        let domain = ZoneDomain {
            l1_chain_id: 42,
            zone_id: index as u32 + 1,
            chain_id: 4_001 + index as u64,
            portal: Address::with_last_byte(index as u8 + 1),
            authority_epoch: 7,
            roster_hash: B256::with_last_byte(index as u8 + 1),
            protocol_version: 1,
        };
        EpochRoster::from_finalized_registry(
            domain,
            [
                signers[index].address(),
                Address::with_last_byte(index as u8 + 10),
                Address::with_last_byte(index as u8 + 20),
            ],
        )
        .unwrap()
    });
    let mut proofs = std::array::from_fn(|index| TransportSessionProof {
        request_id: B256::repeat_byte(1),
        initiator_ed25519: B256::repeat_byte(2),
        responder_ed25519: B256::repeat_byte(3),
        initiator: rosters[0].domain,
        initiator_member: signers[0].address(),
        responder: rosters[1].domain,
        responder_member: signers[1].address(),
        initiator_nonce: B256::repeat_byte(4),
        responder_nonce: B256::repeat_byte(5),
        stream: 6,
        sequence: 7,
        responder_role: index == 1,
        signature: SignatureBytes([0; 65]),
    });
    sign(&signers, &mut proofs);
    (rosters, signers, proofs)
}

fn sign(signers: &[PrivateKeySigner; 2], proofs: &mut [TransportSessionProof; 2]) {
    for (signer, proof) in signers.iter().zip(proofs) {
        proof.signature = SignatureBytes(
            signer
                .sign_hash_sync(&proof.session_hash())
                .unwrap()
                .as_bytes(),
        );
    }
}

fn establish(
    rosters: &[EpochRoster; 2],
    proofs: &[TransportSessionProof; 2],
) -> Result<AuthenticatedPeerSession, TransportError> {
    AuthenticatedPeerSession::establish(
        rosters[0].clone(),
        rosters[1].clone(),
        &proofs[0],
        &proofs[1],
    )
}

#[test]
fn exact_signed_noise_pair_and_frame_position_round_trip() {
    let (rosters, _, proofs) = fixture();
    for proof in &proofs {
        let bytes = proof.canonical_bytes();
        assert_eq!(TransportSessionProof::decode(&bytes).unwrap(), *proof);
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(TransportSessionProof::decode(&trailing).is_err());
        assert!(TransportSessionProof::decode(&bytes[..bytes.len() - 1]).is_err());
    }
    let session = establish(&rosters, &proofs).unwrap();
    assert_eq!(session.request_id(), proofs[0].request_id);
    assert!(session.authenticates_delivery(6, 7));
    assert!(!session.authenticates_delivery(6, 8));
    assert!(!session.authenticates_delivery(7, 7));
}

#[test]
fn every_channel_binding_mutation_invalidates_existing_signatures() {
    let (rosters, _, original) = fixture();
    let mutations: [fn(&mut TransportSessionProof); 7] = [
        |proof| proof.request_id = B256::repeat_byte(8),
        |proof| proof.initiator_ed25519 = B256::repeat_byte(8),
        |proof| proof.responder_ed25519 = B256::repeat_byte(8),
        |proof| proof.initiator_nonce = B256::repeat_byte(8),
        |proof| proof.responder_nonce = B256::repeat_byte(8),
        |proof| proof.stream += 1,
        |proof| proof.sequence += 1,
    ];
    for mutate in mutations {
        let mut mismatch = original.clone();
        mutate(&mut mismatch[0]);
        assert_eq!(
            establish(&rosters, &mismatch).unwrap_err(),
            TransportError::TranscriptMismatch
        );
        let mut both = original.clone();
        both.iter_mut().for_each(mutate);
        assert_eq!(
            establish(&rosters, &both).unwrap_err(),
            TransportError::Authentication
        );
    }
}

#[test]
fn even_valid_member_signatures_cannot_authorize_empty_or_reflected_transport() {
    let (rosters, signers, original) = fixture();
    let mutations: [fn(&mut TransportSessionProof); 6] = [
        |proof| proof.request_id = B256::ZERO,
        |proof| proof.initiator_ed25519 = B256::ZERO,
        |proof| proof.responder_ed25519 = B256::ZERO,
        |proof| proof.responder_ed25519 = proof.initiator_ed25519,
        |proof| proof.initiator_nonce = B256::ZERO,
        |proof| proof.responder_nonce = B256::ZERO,
    ];
    for mutate in mutations {
        let mut proofs = original.clone();
        proofs.iter_mut().for_each(mutate);
        sign(&signers, &mut proofs);
        assert_eq!(
            establish(&rosters, &proofs).unwrap_err(),
            TransportError::TranscriptMismatch
        );
    }
}
