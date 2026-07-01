//! Chaum-Pedersen DLOG equality proof verification precompile.
//!
//! Registered at [`CHAUM_PEDERSEN_VERIFY_ADDRESS`] (`0x1C00...0100`).
//!
//! Verifies that the sequencer correctly derived the ECDH shared secret
//! from the depositor's ephemeral public key, without revealing the
//! sequencer's private key to the EVM.
//!
//! Uses the NCC-audited [`k256`] crate (v0.13.4) for secp256k1 operations.

use alloc::vec::Vec;

mod dispatch;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, address};
use k256::{
    AffinePoint, ProjectivePoint, Scalar,
    elliptic_curve::{
        ops::Reduce,
        sec1::{FromEncodedPoint, ToEncodedPoint},
    },
};
use revm::precompile::PrecompileId;

/// Chaum-Pedersen Verify precompile address on Zone L2.
pub const CHAUM_PEDERSEN_VERIFY_ADDRESS: Address =
    address!("0x1C00000000000000000000000000000000000100");

/// Gas cost for Chaum-Pedersen proof verification (two EC muls + hashing).
const CP_VERIFY_GAS: u64 = 6_000;

alloy_sol_types::sol! {
    /// Chaum-Pedersen proof for ECDH shared secret derivation.
    struct ChaumPedersenProof {
        bytes32 s;
        bytes32 c;
    }

    interface IChaumPedersenVerify {
        /// Verify a Chaum-Pedersen proof of correct ECDH shared secret derivation.
        function verifyProof(
            bytes32 ephemeralPubX,
            uint8 ephemeralPubYParity,
            bytes32 sharedSecret,
            uint8 sharedSecretYParity,
            bytes32 sequencerPubX,
            uint8 sequencerPubYParity,
            ChaumPedersenProof proof
        ) external view returns (bool valid);
    }
}

pub use IChaumPedersenVerify::verifyProofCall;

/// Chaum-Pedersen DLOG equality proof verification precompile.
///
/// Verifies that the sequencer knows `privSeq` such that:
/// - `pubSeq = privSeq * G` (their public key)
/// - `sharedSecretPoint = privSeq * ephemeralPub` (the ECDH computation)
///
/// Verification equations:
/// - `R1 = s*G - c*pubSeq`
/// - `R2 = s*ephemeralPub - c*sharedSecretPoint`
/// - `c' = keccak256(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)`
/// - Check: `c == c'`
pub struct ChaumPedersenVerify;

impl ChaumPedersenVerify {
    /// Wrap this precompile in a [`DynPrecompile`] with the Tempo storage context
    /// required by the upstream dispatch macro.
    pub fn create(
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> DynPrecompile {
        use tempo_precompiles::{
            Precompile as _,
            storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
        };

        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();
        DynPrecompile::new_stateful(
            PrecompileId::Custom("ChaumPedersenVerify".into()),
            move |input| {
                let mut storage = EvmPrecompileStorageProvider::new(
                    input.internals,
                    input.gas,
                    input.reservoir,
                    spec,
                    amsterdam_eip8037_enabled,
                    input.is_static,
                    gas_params.clone(),
                );

                StorageCtx::enter(&mut storage, || {
                    let mut precompile = Self;
                    precompile.call(input.data, input.caller)
                })
            },
        )
    }
}

/// Recover a secp256k1 affine point from compressed form (x coordinate + y parity).
///
/// `y_parity` follows SEC1: `0x02` for even y, `0x03` for odd y.
pub fn recover_point(x_bytes: &[u8; 32], y_parity: u8) -> Option<AffinePoint> {
    let mut encoded = [0u8; 33];
    encoded[0] = y_parity;
    encoded[1..].copy_from_slice(x_bytes);

    let point = k256::EncodedPoint::from_bytes(encoded).ok()?;
    Option::from(AffinePoint::from_encoded_point(&point))
}

/// Compute the Chaum-Pedersen challenge hash.
///
/// `c = keccak256(G || ephemeralPub || pubSeq || sharedSecretPoint || R1 || R2)`
///
/// Shared between the verifier (precompile) and prover (ecies module).
pub fn challenge_hash(
    ephemeral_pub: &AffinePoint,
    sequencer_pub: &AffinePoint,
    shared_secret: &AffinePoint,
    r1: &AffinePoint,
    r2: &AffinePoint,
) -> Scalar {
    let g_affine = AffinePoint::from(ProjectivePoint::GENERATOR);

    let mut preimage = Vec::with_capacity(6 * 65); // 6 uncompressed secp256k1 points
    preimage.extend_from_slice(g_affine.to_encoded_point(false).as_bytes());
    preimage.extend_from_slice(ephemeral_pub.to_encoded_point(false).as_bytes());
    preimage.extend_from_slice(sequencer_pub.to_encoded_point(false).as_bytes());
    preimage.extend_from_slice(shared_secret.to_encoded_point(false).as_bytes());
    preimage.extend_from_slice(r1.to_encoded_point(false).as_bytes());
    preimage.extend_from_slice(r2.to_encoded_point(false).as_bytes());

    let hash = alloy_primitives::keccak256(&preimage);
    <Scalar as Reduce<k256::U256>>::reduce_bytes(&hash.0.into())
}

/// Verify a Chaum-Pedersen DLOG equality proof on secp256k1.
///
/// Proves knowledge of scalar `x` such that `pubSeq = x*G` AND `sharedSecret = x*ephemeralPub`.
fn verify_chaum_pedersen(
    ephemeral_pub_x: &[u8; 32],
    ephemeral_pub_y_parity: u8,
    shared_secret_x: &[u8; 32],
    shared_secret_y_parity: u8,
    sequencer_pub_x: &[u8; 32],
    sequencer_pub_y_parity: u8,
    s_bytes: &[u8; 32],
    c_bytes: &[u8; 32],
) -> bool {
    // Recover points
    let Some(ephemeral_pub) = recover_point(ephemeral_pub_x, ephemeral_pub_y_parity) else {
        return false;
    };
    let Some(shared_secret_point) = recover_point(shared_secret_x, shared_secret_y_parity) else {
        return false;
    };
    let Some(sequencer_pub) = recover_point(sequencer_pub_x, sequencer_pub_y_parity) else {
        return false;
    };

    // Deserialize proof scalars by reducing modulo the group order.
    let s = <Scalar as Reduce<k256::U256>>::reduce_bytes(&(*s_bytes).into());
    let c = <Scalar as Reduce<k256::U256>>::reduce_bytes(&(*c_bytes).into());

    // R1 = s*G - c*pubSeq
    let r1 = ProjectivePoint::GENERATOR * s - ProjectivePoint::from(sequencer_pub) * c;

    // R2 = s*ephemeralPub - c*sharedSecretPoint
    let r2 =
        ProjectivePoint::from(ephemeral_pub) * s - ProjectivePoint::from(shared_secret_point) * c;

    let r1_affine = AffinePoint::from(r1);
    let r2_affine = AffinePoint::from(r2);

    // Recompute challenge and compare
    let c_prime = challenge_hash(
        &ephemeral_pub,
        &sequencer_pub,
        &shared_secret_point,
        &r1_affine,
        &r2_affine,
    );

    c == c_prime
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::elliptic_curve::{Field, PrimeField};

    #[test]
    fn test_recover_point_generator() {
        let g = AffinePoint::from(ProjectivePoint::GENERATOR);
        let encoded = g.to_encoded_point(true);
        let x: [u8; 32] = encoded.x().unwrap().as_slice().try_into().unwrap();
        let parity = encoded.as_bytes()[0];

        let recovered = recover_point(&x, parity).expect("should recover generator");
        assert_eq!(recovered, g);
    }

    #[test]
    fn test_chaum_pedersen_valid_proof() {
        let mut rng = rand::thread_rng();
        let priv_seq = Scalar::random(&mut rng);
        let pub_seq = (ProjectivePoint::GENERATOR * priv_seq).to_affine();

        let eph_priv = Scalar::random(&mut rng);
        let eph_pub = (ProjectivePoint::GENERATOR * eph_priv).to_affine();

        let shared_secret = (ProjectivePoint::from(eph_pub) * priv_seq).to_affine();

        // Generate proof
        let k = Scalar::random(&mut rng);
        let r1 = (ProjectivePoint::GENERATOR * k).to_affine();
        let r2 = (ProjectivePoint::from(eph_pub) * k).to_affine();

        let c = challenge_hash(&eph_pub, &pub_seq, &shared_secret, &r1, &r2);
        let s = k + c * priv_seq;

        let eph_enc = eph_pub.to_encoded_point(true);
        let ss_enc = shared_secret.to_encoded_point(true);
        let ps_enc = pub_seq.to_encoded_point(true);

        let valid = verify_chaum_pedersen(
            eph_enc.x().unwrap().as_slice().try_into().unwrap(),
            eph_enc.as_bytes()[0],
            ss_enc.x().unwrap().as_slice().try_into().unwrap(),
            ss_enc.as_bytes()[0],
            ps_enc.x().unwrap().as_slice().try_into().unwrap(),
            ps_enc.as_bytes()[0],
            &s.to_repr().into(),
            &c.to_repr().into(),
        );

        assert!(valid, "valid Chaum-Pedersen proof should verify");
    }

    #[test]
    fn test_chaum_pedersen_invalid_proof() {
        let mut rng = rand::thread_rng();
        let priv_seq = Scalar::random(&mut rng);
        let pub_seq = (ProjectivePoint::GENERATOR * priv_seq).to_affine();

        let eph_priv = Scalar::random(&mut rng);
        let eph_pub = (ProjectivePoint::GENERATOR * eph_priv).to_affine();
        let shared_secret = (ProjectivePoint::from(eph_pub) * priv_seq).to_affine();

        let eph_enc = eph_pub.to_encoded_point(true);
        let ss_enc = shared_secret.to_encoded_point(true);
        let ps_enc = pub_seq.to_encoded_point(true);

        let valid = verify_chaum_pedersen(
            eph_enc.x().unwrap().as_slice().try_into().unwrap(),
            eph_enc.as_bytes()[0],
            ss_enc.x().unwrap().as_slice().try_into().unwrap(),
            ss_enc.as_bytes()[0],
            ps_enc.x().unwrap().as_slice().try_into().unwrap(),
            ps_enc.as_bytes()[0],
            &[0xAAu8; 32],
            &[0xBBu8; 32],
        );

        assert!(!valid, "invalid proof should not verify");
    }

    #[test]
    fn test_recover_point_invalid_parity() {
        let g = AffinePoint::from(ProjectivePoint::GENERATOR);
        let encoded = g.to_encoded_point(true);
        let x: [u8; 32] = encoded.x().unwrap().as_slice().try_into().unwrap();

        assert!(recover_point(&x, 0x00).is_none());
        assert!(recover_point(&x, 0x04).is_none());
        assert!(recover_point(&x, 0xFF).is_none());
    }

    #[test]
    fn test_chaum_pedersen_tampered_s() {
        let mut rng = rand::thread_rng();
        let priv_seq = Scalar::random(&mut rng);
        let pub_seq = (ProjectivePoint::GENERATOR * priv_seq).to_affine();
        let eph_priv = Scalar::random(&mut rng);
        let eph_pub = (ProjectivePoint::GENERATOR * eph_priv).to_affine();
        let shared_secret = (ProjectivePoint::from(eph_pub) * priv_seq).to_affine();
        let k = Scalar::random(&mut rng);
        let r1 = (ProjectivePoint::GENERATOR * k).to_affine();
        let r2 = (ProjectivePoint::from(eph_pub) * k).to_affine();
        let c = challenge_hash(&eph_pub, &pub_seq, &shared_secret, &r1, &r2);
        let s = k + c * priv_seq;

        let s_tampered = s + Scalar::ONE;

        let eph_enc = eph_pub.to_encoded_point(true);
        let ss_enc = shared_secret.to_encoded_point(true);
        let ps_enc = pub_seq.to_encoded_point(true);

        let valid = verify_chaum_pedersen(
            eph_enc.x().unwrap().as_slice().try_into().unwrap(),
            eph_enc.as_bytes()[0],
            ss_enc.x().unwrap().as_slice().try_into().unwrap(),
            ss_enc.as_bytes()[0],
            ps_enc.x().unwrap().as_slice().try_into().unwrap(),
            ps_enc.as_bytes()[0],
            &s_tampered.to_repr().into(),
            &c.to_repr().into(),
        );

        assert!(!valid, "tampered s should not verify");
    }

    #[test]
    fn test_chaum_pedersen_tampered_c() {
        let mut rng = rand::thread_rng();
        let priv_seq = Scalar::random(&mut rng);
        let pub_seq = (ProjectivePoint::GENERATOR * priv_seq).to_affine();
        let eph_priv = Scalar::random(&mut rng);
        let eph_pub = (ProjectivePoint::GENERATOR * eph_priv).to_affine();
        let shared_secret = (ProjectivePoint::from(eph_pub) * priv_seq).to_affine();
        let k = Scalar::random(&mut rng);
        let r1 = (ProjectivePoint::GENERATOR * k).to_affine();
        let r2 = (ProjectivePoint::from(eph_pub) * k).to_affine();
        let c = challenge_hash(&eph_pub, &pub_seq, &shared_secret, &r1, &r2);
        let s = k + c * priv_seq;

        let c_tampered = c + Scalar::ONE;

        let eph_enc = eph_pub.to_encoded_point(true);
        let ss_enc = shared_secret.to_encoded_point(true);
        let ps_enc = pub_seq.to_encoded_point(true);

        let valid = verify_chaum_pedersen(
            eph_enc.x().unwrap().as_slice().try_into().unwrap(),
            eph_enc.as_bytes()[0],
            ss_enc.x().unwrap().as_slice().try_into().unwrap(),
            ss_enc.as_bytes()[0],
            ps_enc.x().unwrap().as_slice().try_into().unwrap(),
            ps_enc.as_bytes()[0],
            &s.to_repr().into(),
            &c_tampered.to_repr().into(),
        );

        assert!(!valid, "tampered c should not verify");
    }

    #[test]
    fn test_chaum_pedersen_wrong_shared_secret_parity() {
        let mut rng = rand::thread_rng();
        let priv_seq = Scalar::random(&mut rng);
        let pub_seq = (ProjectivePoint::GENERATOR * priv_seq).to_affine();
        let eph_priv = Scalar::random(&mut rng);
        let eph_pub = (ProjectivePoint::GENERATOR * eph_priv).to_affine();
        let shared_secret = (ProjectivePoint::from(eph_pub) * priv_seq).to_affine();
        let k = Scalar::random(&mut rng);
        let r1 = (ProjectivePoint::GENERATOR * k).to_affine();
        let r2 = (ProjectivePoint::from(eph_pub) * k).to_affine();
        let c = challenge_hash(&eph_pub, &pub_seq, &shared_secret, &r1, &r2);
        let s = k + c * priv_seq;

        let eph_enc = eph_pub.to_encoded_point(true);
        let ss_enc = shared_secret.to_encoded_point(true);
        let ps_enc = pub_seq.to_encoded_point(true);

        let ss_parity = ss_enc.as_bytes()[0];
        let flipped_ss_parity = if ss_parity == 0x02 { 0x03 } else { 0x02 };

        let valid = verify_chaum_pedersen(
            eph_enc.x().unwrap().as_slice().try_into().unwrap(),
            eph_enc.as_bytes()[0],
            ss_enc.x().unwrap().as_slice().try_into().unwrap(),
            flipped_ss_parity,
            ps_enc.x().unwrap().as_slice().try_into().unwrap(),
            ps_enc.as_bytes()[0],
            &s.to_repr().into(),
            &c.to_repr().into(),
        );

        assert!(!valid, "wrong shared secret parity should not verify");
    }

    #[test]
    fn test_chaum_pedersen_wrong_ephemeral_parity() {
        let mut rng = rand::thread_rng();
        let priv_seq = Scalar::random(&mut rng);
        let pub_seq = (ProjectivePoint::GENERATOR * priv_seq).to_affine();
        let eph_priv = Scalar::random(&mut rng);
        let eph_pub = (ProjectivePoint::GENERATOR * eph_priv).to_affine();
        let shared_secret = (ProjectivePoint::from(eph_pub) * priv_seq).to_affine();
        let k = Scalar::random(&mut rng);
        let r1 = (ProjectivePoint::GENERATOR * k).to_affine();
        let r2 = (ProjectivePoint::from(eph_pub) * k).to_affine();
        let c = challenge_hash(&eph_pub, &pub_seq, &shared_secret, &r1, &r2);
        let s = k + c * priv_seq;

        let eph_enc = eph_pub.to_encoded_point(true);
        let ss_enc = shared_secret.to_encoded_point(true);
        let ps_enc = pub_seq.to_encoded_point(true);

        let eph_parity = eph_enc.as_bytes()[0];
        let flipped_eph_parity = if eph_parity == 0x02 { 0x03 } else { 0x02 };

        let valid = verify_chaum_pedersen(
            eph_enc.x().unwrap().as_slice().try_into().unwrap(),
            flipped_eph_parity,
            ss_enc.x().unwrap().as_slice().try_into().unwrap(),
            ss_enc.as_bytes()[0],
            ps_enc.x().unwrap().as_slice().try_into().unwrap(),
            ps_enc.as_bytes()[0],
            &s.to_repr().into(),
            &c.to_repr().into(),
        );

        assert!(!valid, "wrong ephemeral pubkey parity should not verify");
    }

    #[test]
    fn test_chaum_pedersen_identity_r1_r2() {
        let mut rng = rand::thread_rng();
        let priv_seq = Scalar::random(&mut rng);
        let pub_seq = (ProjectivePoint::GENERATOR * priv_seq).to_affine();
        let eph_priv = Scalar::random(&mut rng);
        let eph_pub = (ProjectivePoint::GENERATOR * eph_priv).to_affine();
        let shared_secret = (ProjectivePoint::from(eph_pub) * priv_seq).to_affine();

        let c = Scalar::random(&mut rng);
        let s = c * priv_seq;

        let eph_enc = eph_pub.to_encoded_point(true);
        let ss_enc = shared_secret.to_encoded_point(true);
        let ps_enc = pub_seq.to_encoded_point(true);

        let valid = verify_chaum_pedersen(
            eph_enc.x().unwrap().as_slice().try_into().unwrap(),
            eph_enc.as_bytes()[0],
            ss_enc.x().unwrap().as_slice().try_into().unwrap(),
            ss_enc.as_bytes()[0],
            ps_enc.x().unwrap().as_slice().try_into().unwrap(),
            ps_enc.as_bytes()[0],
            &s.to_repr().into(),
            &c.to_repr().into(),
        );

        assert!(
            !valid,
            "degenerate proof with identity R1/R2 should not verify"
        );
    }
}
