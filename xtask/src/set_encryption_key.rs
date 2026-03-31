//! Registers the sequencer's encryption key on the ZonePortal.
//!
//! Derives the secp256k1 public key from the private key, constructs a
//! proof-of-possession (POP) ECDSA signature, and calls
//! `setSequencerEncryptionKey` on the portal contract.

use alloy::{
    network::{EthereumWallet, primitives::ReceiptResponse},
    primitives::{Address, B256, U256, keccak256},
    providers::ProviderBuilder,
    signers::{Signer, local::PrivateKeySigner},
    sol_types::SolValue,
};
use eyre::{WrapErr as _, eyre};
use k256::{AffinePoint, ProjectivePoint, Scalar, elliptic_curve::sec1::ToEncodedPoint};
use tempo_alloy::TempoNetwork;
use zone::abi::ZonePortal;

#[derive(Debug, clap::Parser)]
pub(crate) struct SetEncryptionKey {
    /// Tempo L1 RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// ZonePortal contract address on Tempo L1.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Address,

    /// Sequencer private key (hex). Used both as the signing key for the
    /// transaction and as the encryption key to register.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: String,
}

impl SetEncryptionKey {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let key_str = self
            .private_key
            .strip_prefix("0x")
            .unwrap_or(&self.private_key);

        // The sequencer key is used both to sign the tx and as the encryption key
        let signer: PrivateKeySigner = key_str.parse()?;

        // Derive compressed public key coordinates
        let enc_key = k256::SecretKey::from_slice(&const_hex::decode(key_str)?)?;
        let scalar: Scalar = *enc_key.to_nonzero_scalar();
        let pub_point = AffinePoint::from(ProjectivePoint::GENERATOR * scalar);
        let encoded = pub_point.to_encoded_point(true);
        let x = B256::from_slice(encoded.x().unwrap().as_slice());
        let y_parity: u8 = encoded.as_bytes()[0]; // 0x02 or 0x03

        println!("Encryption key x: {x}");
        println!("Encryption key yParity: {y_parity:#x}");

        // Build POP message and sign BEFORE moving signer into wallet
        let message = keccak256((self.portal, x, U256::from(y_parity)).abi_encode());
        let sig = signer.sign_hash(&message).await?;

        let pop_v = sig.v() as u8 + 27;
        let pop_r = B256::from(sig.r().to_be_bytes::<32>());
        let pop_s = B256::from(sig.s().to_be_bytes::<32>());

        // Now move signer into wallet (no clone needed)
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(wallet)
            .connect(&self.l1_rpc_url)
            .await?;

        println!(
            "Sending setSequencerEncryptionKey to portal {}...",
            self.portal
        );
        let portal = ZonePortal::new(self.portal, &provider);
        let receipt = portal
            .setSequencerEncryptionKey(x, y_parity, pop_v, pop_r, pop_s)
            .send_sync()
            .await
            .wrap_err("failed to send setSequencerEncryptionKey")?;

        let tx_hash = receipt.transaction_hash;
        if !receipt.status() {
            return Err(eyre!("setSequencerEncryptionKey reverted (tx: {tx_hash})"));
        }

        println!("Encryption key registered!");
        println!("Explorer: https://explore.moderato.tempo.xyz/tx/{tx_hash}");

        Ok(())
    }
}
