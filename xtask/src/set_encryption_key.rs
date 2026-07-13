//! Registers the sequencer's encryption key on the ZonePortal.
//!
//! Calls the shared sequencer registration helper, which derives the secp256k1
//! public key, constructs the proof-of-possession signature, and submits it to
//! the portal contract.

use alloy::{
    network::EthereumWallet, primitives::Address, providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
};
use eyre::WrapErr as _;
use tempo_alloy::TempoNetwork;
use zone_sequencer::register_encryption_key;

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

        let wallet = EthereumWallet::from(signer.clone());
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(wallet)
            .connect(&self.l1_rpc_url)
            .await?;

        println!(
            "Sending setSequencerEncryptionKey to portal {}...",
            self.portal
        );
        let tx_hash = register_encryption_key(&provider, self.portal, &signer)
            .await
            .wrap_err("failed to send setSequencerEncryptionKey")?;

        println!("Encryption key registered!");
        println!("Explorer: https://explore.moderato.tempo.xyz/tx/{tx_hash}");

        Ok(())
    }
}
