//! Sequencer encryption-key registration on Tempo L1.

use alloy_network::ReceiptResponse as _;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolValue;
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::ZonePortal;

/// Registers `signer` as the sequencer encryption key on `portal`.
///
/// Derives the secp256k1 public key, signs a proof-of-possession over
/// `(portal, x, yParity)`, and returns the registration transaction hash.
pub async fn register_encryption_key<P: Provider<TempoNetwork>>(
    provider: &P,
    portal: Address,
    signer: &PrivateKeySigner,
) -> eyre::Result<B256> {
    use k256::{AffinePoint, ProjectivePoint, Scalar, elliptic_curve::sec1::ToEncodedPoint};

    let secret = k256::SecretKey::from_slice(signer.to_bytes().as_slice())?;
    let scalar: Scalar = *secret.to_nonzero_scalar();
    let public = AffinePoint::from(ProjectivePoint::GENERATOR * scalar);
    let encoded = public.to_encoded_point(true);
    let x = B256::from_slice(encoded.x().expect("compressed point has x").as_slice());
    let y_parity: u8 = encoded.as_bytes()[0]; // 0x02 or 0x03

    let message = keccak256((portal, x, U256::from(y_parity)).abi_encode());
    let signature = signer.sign_hash_sync(&message)?;
    let pop_v = signature.v() as u8 + 27;
    let pop_r = B256::from(signature.r().to_be_bytes::<32>());
    let pop_s = B256::from(signature.s().to_be_bytes::<32>());

    let receipt = ZonePortal::new(portal, provider)
        .setSequencerEncryptionKey(x, y_parity, pop_v, pop_r, pop_s)
        .send()
        .await?
        .get_receipt()
        .await?;
    let tx_hash = receipt.transaction_hash();
    eyre::ensure!(
        receipt.status(),
        "setSequencerEncryptionKey reverted (tx: {tx_hash})"
    );
    Ok(tx_hash)
}
