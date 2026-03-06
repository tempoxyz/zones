//! Sends an encrypted deposit to the ZonePortal on Tempo L1.
//!
//! Encrypts `(to, memo)` using the sequencer's ECIES public key so the
//! recipient and memo are hidden from on-chain observers.

use alloy::{
    network::{EthereumWallet, primitives::ReceiptResponse},
    primitives::{Address, B256, Bytes, address},
    providers::{Provider, ProviderBuilder},
    rpc::types::Filter,
    signers::local::PrivateKeySigner,
    sol_types::SolEvent,
};
use eyre::{WrapErr as _, eyre};
use tempo_alloy::TempoNetwork;
use zone::{
    abi::{EncryptedDepositPayload, ZoneInbox, ZonePortal},
    precompiles::ecies::encrypt_deposit,
};

#[derive(Debug, clap::Parser)]
pub(crate) struct EncryptedDeposit {
    /// Tempo L1 RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// ZonePortal contract address on Tempo L1.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Address,

    /// Private key (hex) for signing the deposit transaction.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: String,

    /// TIP-20 token address to deposit.
    #[arg(long, default_value_t = address!("0x20C0000000000000000000000000000000000000"))]
    token: Address,

    /// Amount to deposit.
    #[arg(long)]
    amount: u128,

    /// Recipient address on the zone (encrypted on-chain).
    #[arg(long)]
    to: Option<Address>,

    /// Memo bytes32 (encrypted on-chain).
    #[arg(long, default_value_t = B256::ZERO)]
    memo: B256,

    /// Zone L2 RPC URL. If set, waits for the deposit to be processed on L2.
    #[arg(long, env = "ZONE_RPC_URL")]
    zone_rpc_url: Option<String>,
}

impl EncryptedDeposit {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let key_str = self
            .private_key
            .strip_prefix("0x")
            .unwrap_or(&self.private_key);
        let signer: PrivateKeySigner = key_str.parse()?;
        let sender = signer.address();
        let to = self.to.unwrap_or(sender);
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(wallet)
            .connect(&self.l1_rpc_url)
            .await?;

        // Capture L2 block before sending so we don't miss the event
        let l2_from_block = if let Some(ref zone_rpc) = self.zone_rpc_url {
            let l2 = ProviderBuilder::new().connect(zone_rpc).await?;
            Some(l2.get_block_number().await.unwrap_or(0))
        } else {
            None
        };

        let portal = ZonePortal::new(self.portal, &provider);

        // Fetch sequencer encryption key
        println!("Fetching sequencer encryption key...");
        let (key, key_index) = portal
            .encryption_key()
            .await
            .wrap_err("failed to fetch encryption key — is one set?")?;

        let seq_pub_x = key.x;
        let seq_pub_y_parity = key.normalized_y_parity().ok_or_else(|| {
            eyre!(
                "unexpected yParity {:#x}, expected 0/1 or 0x02/0x03",
                key.yParity
            )
        })?;
        println!(
            "Encryption key index: {key_index}, x: {seq_pub_x}, parity: {seq_pub_y_parity:#x}"
        );

        // Encrypt (to, memo) to the sequencer's public key
        let enc = encrypt_deposit(
            &seq_pub_x,
            seq_pub_y_parity,
            to,
            self.memo,
            self.portal,
            key_index,
        )
        .ok_or_else(|| eyre!("ECIES encryption failed — invalid sequencer public key?"))?;

        let payload = EncryptedDepositPayload {
            ephemeralPubkeyX: enc.eph_pub_x,
            ephemeralPubkeyYParity: enc.eph_pub_y_parity,
            ciphertext: Bytes::from(enc.ciphertext),
            nonce: enc.nonce.into(),
            tag: enc.tag.into(),
        };

        println!("Sending encrypted deposit of {} to {to}...", self.amount);
        let receipt = portal
            .depositEncrypted(self.token, self.amount, key_index, payload)
            .send_sync()
            .await
            .wrap_err("failed to send depositEncrypted transaction")?;
        let tx_hash = receipt.transaction_hash;
        let block_number = receipt.block_number.unwrap_or_default();

        if !receipt.status() {
            return Err(eyre!("depositEncrypted reverted (tx: {tx_hash})"));
        }

        println!("Encrypted deposit sent! (block {block_number})");
        println!("Explorer: https://explore.moderato.tempo.xyz/tx/{tx_hash}");

        // Wait for L2 processing if zone RPC is provided
        if let (Some(zone_rpc), Some(from_block)) = (&self.zone_rpc_url, l2_from_block) {
            self.wait_for_l2_processing(zone_rpc, from_block, sender, to)
                .await?;
        }

        Ok(())
    }

    /// Poll the zone L2 for the `EncryptedDepositProcessed` or `EncryptedDepositFailed` event.
    async fn wait_for_l2_processing(
        &self,
        zone_rpc: &str,
        from_block: u64,
        sender: Address,
        to: Address,
    ) -> eyre::Result<()> {
        use zone::abi::ZONE_INBOX_ADDRESS;

        println!("Waiting for encrypted deposit to be processed on L2...");
        let l2 = ProviderBuilder::new().connect(zone_rpc).await?;

        let processed_filter = Filter::new()
            .address(ZONE_INBOX_ADDRESS)
            .event_signature(ZoneInbox::EncryptedDepositProcessed::SIGNATURE_HASH)
            .from_block(from_block);

        let failed_filter = Filter::new()
            .address(ZONE_INBOX_ADDRESS)
            .event_signature(ZoneInbox::EncryptedDepositFailed::SIGNATURE_HASH)
            .from_block(from_block);

        loop {
            // Check for successful processing
            let logs = l2.get_logs(&processed_filter).await.unwrap_or_default();
            for log in &logs {
                if let Ok(event) = ZoneInbox::EncryptedDepositProcessed::decode_log(&log.inner)
                    && event.data.sender == sender
                    && event.data.to == to
                {
                    let block = log.block_number.unwrap_or(0);
                    println!("Encrypted deposit processed on L2! (block {block})");
                    println!("  Token:  {}", event.data.token);
                    println!("  Sender: {}", event.data.sender);
                    println!("  To:     {}", event.data.to);
                    println!("  Amount: {}", event.data.amount);
                    println!("  Memo:   {}", event.data.memo);
                    return Ok(());
                }
            }

            // Check for failure
            let failed_logs = l2.get_logs(&failed_filter).await.unwrap_or_default();
            for log in &failed_logs {
                if let Ok(event) = ZoneInbox::EncryptedDepositFailed::decode_log(&log.inner)
                    && event.data.sender == sender
                {
                    let block = log.block_number.unwrap_or(0);
                    println!(
                        "WARNING: Encrypted deposit FAILED on L2 (block {block}). \
                         Funds returned to sender."
                    );
                    println!("  Token:  {}", event.data.token);
                    println!("  Sender: {}", event.data.sender);
                    println!("  Amount: {}", event.data.amount);
                    return Ok(());
                }
            }

            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}
