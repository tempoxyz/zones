//! Sends an encrypted deposit to the ZonePortal on Tempo L1.
//!
//! Encrypts `(to, memo)` using the sequencer's ECIES public key so the
//! recipient and memo are hidden from on-chain observers.

use alloy::{
    network::{EthereumWallet, primitives::ReceiptResponse},
    primitives::{Address, B256, U256, address},
    providers::{Provider, ProviderBuilder},
};
use eyre::{WrapErr as _, eyre};
use tempo_alloy::TempoNetwork;
use zone::abi::ZonePortal;

use crate::{
    bridge_utils::{
        BuiltEncryptedDepositPayload, EncryptionKeyMode, build_encrypted_deposit_payload,
        parse_private_key, read_payload_json, resolve_zone_ref,
    },
    zone_utils::{EncryptedDepositResult, wait_for_encrypted_deposit_result},
};

#[derive(Debug, clap::Parser)]
pub(crate) struct EncryptedDeposit {
    /// Tempo L1 RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// ZonePortal contract address on Tempo L1.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Option<Address>,

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

    /// Read a prebuilt encrypted payload JSON file from disk, or '-' for stdin.
    #[arg(long)]
    payload_file: Option<String>,

    /// Zone L2 RPC URL. If set, waits for the deposit to be processed on L2.
    #[arg(long, env = "ZONE_RPC_URL")]
    zone_rpc_url: Option<String>,
}

impl EncryptedDeposit {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let signer = parse_private_key(&self.private_key)?;
        let sender = signer.address();
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

        let default_to = self.to.unwrap_or(sender);
        let (portal_address, key_index, payload, wait_to, wait_memo) =
            if let Some(payload_file) = self.payload_file.as_deref() {
                resolve_prebuilt_payload(self.portal, self.to, read_payload_json(payload_file)?)?
            } else {
                let portal_address = self.portal.ok_or_else(|| {
                    eyre!("portal address is required unless --payload-file is provided")
                })?;
                let resolved_zone =
                    resolve_zone_ref(&portal_address.to_string(), &provider, None).await?;
                let built = build_encrypted_deposit_payload(
                    &provider,
                    portal_address,
                    default_to,
                    self.memo,
                    EncryptionKeyMode::ReadOnly {
                        expected_sequencer_private_key: resolved_zone.sequencer_key.as_deref(),
                    },
                )
                .await?;
                (
                    portal_address,
                    built.key_index,
                    built.encrypted_deposit_payload,
                    Some(default_to),
                    Some(self.memo),
                )
            };

        let portal = ZonePortal::new(portal_address, &provider);

        println!(
            "Sending encrypted deposit of {} to portal {}...",
            self.amount, portal_address
        );
        if let Some(wait_to) = wait_to {
            println!("  Expected recipient on zone: {wait_to}");
        };
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
            self.wait_for_l2_processing(
                zone_rpc, from_block, sender, wait_to, self.token, wait_memo,
            )
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
        to: Option<Address>,
        token: Address,
        memo: Option<B256>,
    ) -> eyre::Result<()> {
        println!("Waiting for encrypted deposit to be processed on L2...");
        let l2 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(zone_rpc)
            .await?;
        match wait_for_encrypted_deposit_result(&l2, from_block, sender, to, Some(token), memo)
            .await?
        {
            EncryptedDepositResult::Processed {
                block,
                token,
                sender,
                to,
                amount,
                memo,
            } => {
                println!("Encrypted deposit processed on L2! (block {block})");
                println!("  Token:  {token}");
                println!("  Sender: {sender}");
                println!("  To:     {to}");
                println!("  Amount: {amount}");
                println!("  Memo:   {memo}");
            }
            EncryptedDepositResult::Failed {
                block,
                token,
                sender,
                amount,
            } => {
                println!(
                    "WARNING: Encrypted deposit FAILED on L2 (block {block}). Funds returned to sender."
                );
                println!("  Token:  {token}");
                println!("  Sender: {sender}");
                println!("  Amount: {amount}");
            }
        }

        Ok(())
    }
}

fn resolve_prebuilt_payload(
    cli_portal: Option<Address>,
    cli_to: Option<Address>,
    built: BuiltEncryptedDepositPayload,
) -> eyre::Result<(
    Address,
    U256,
    zone::abi::EncryptedDepositPayload,
    Option<Address>,
    Option<B256>,
)> {
    let portal_address = match cli_portal {
        Some(portal) if portal != built.target_portal => {
            return Err(eyre!(
                "--portal {} does not match targetPortal {} from payload file",
                portal,
                built.target_portal
            ));
        }
        Some(portal) => portal,
        None => built.target_portal,
    };

    Ok((
        portal_address,
        built.key_index,
        built.encrypted_deposit_payload,
        cli_to,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge_utils::{BuiltEncryptedDepositPayloadJson, payload_json_to_string};
    use clap::Parser as _;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    use zone::abi::EncryptedDepositPayload;

    fn temp_payload_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tempo-xtask-encrypted-deposit-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    fn sample_payload() -> eyre::Result<BuiltEncryptedDepositPayload> {
        Ok(BuiltEncryptedDepositPayload {
            target_portal: "0x9000000000000000000000000000000000000009".parse()?,
            key_index: U256::from(7),
            encrypted_deposit_payload: EncryptedDepositPayload {
                ephemeralPubkeyX:
                    "0x0000000000000000000000000000000000000000000000000000000000001234".parse()?,
                ephemeralPubkeyYParity: 3,
                ciphertext: vec![0xaa, 0xbb, 0xcc].into(),
                nonce: [0x11; 12].into(),
                tag: [0x22; 16].into(),
            },
        })
    }

    #[test]
    fn payload_file_args_parse_without_portal() -> eyre::Result<()> {
        let path = temp_payload_path();
        fs::write(&path, payload_json_to_string(&sample_payload()?)?)?;

        let args = EncryptedDeposit::try_parse_from([
            "tempo-xtask",
            "--l1-rpc-url",
            "http://127.0.0.1:8545",
            "--private-key",
            "0x59c6995e998f97a5a0044966f094538e7dca4a5c7f75b61e8eac2e5c9c1b06f2",
            "--amount",
            "1000000",
            "--payload-file",
            path.to_str().unwrap(),
        ])?;

        assert_eq!(args.payload_file.as_deref(), path.to_str());
        assert!(args.portal.is_none());
        Ok(())
    }

    #[test]
    fn prebuilt_payload_branch_reuses_file_contents() -> eyre::Result<()> {
        let path = temp_payload_path();
        let expected = sample_payload()?;
        fs::write(&path, payload_json_to_string(&expected)?)?;

        let round_tripped: BuiltEncryptedDepositPayloadJson =
            serde_json::from_str(&fs::read_to_string(&path)?)?;
        let built = round_tripped.into_built()?;
        let (portal, key_index, payload, wait_to, wait_memo) = resolve_prebuilt_payload(
            None,
            Some("0x1000000000000000000000000000000000000001".parse()?),
            built,
        )?;

        assert_eq!(portal, expected.target_portal);
        assert_eq!(key_index, expected.key_index);
        assert_eq!(
            payload.ciphertext,
            expected.encrypted_deposit_payload.ciphertext
        );
        assert_eq!(
            wait_to,
            Some("0x1000000000000000000000000000000000000001".parse()?)
        );
        assert_eq!(wait_memo, None);
        Ok(())
    }
}
