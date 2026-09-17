//! Submit an encrypted full-balance forced exit using only Tempo L1 RPC.
use alloy::{
    consensus::BlockHeader as _,
    network::{EthereumWallet, primitives::ReceiptResponse},
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    signers::{SignerSync, local::PrivateKeySigner},
    sol_types::SolEvent,
};
use eyre::{WrapErr as _, ensure, eyre};
use k256::elliptic_curve::rand_core::{OsRng, RngCore};
use std::time::Duration;
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::ITIP20;
use tempo_zone_contracts::{ForcedExitAuthorization, ZonePortal};
use zone_precompiles::ecies::{EncryptionContext, encrypt_request};
use zone_primitives::constants::zone_chain_id;

/// Request withdrawal of the account's full liquid token balance, without an L2 transaction.
/// Admission charges compensation even if Zone execution later rejects the request.
#[derive(clap::Parser)]
#[command(
    after_help = "Example (PRIVATE_KEY contains the account root key):\n  cargo run -p tempo-xtask -- forced-withdraw --l1-rpc-url http://localhost:8545 --portal <PORTAL> --to <RECIPIENT> --approve --wait-for-processing\n\nOnly L1 RPC is required. The amount is the full liquid balance at execution time.\n--wait-for-processing observes inbox settlement, not successful L1 delivery."
)]
pub(crate) struct ForcedWithdraw {
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Address,
    /// Account root secp256k1 key; access keys cannot authorize forced withdrawals.
    #[arg(long, env = "PRIVATE_KEY", hide_env_values = true)]
    private_key: String,
    /// Optional separate L1 transaction signer and compensation payer.
    #[arg(long, env = "FEE_PAYER_PRIVATE_KEY", hide_env_values = true)]
    fee_payer_private_key: Option<String>,
    #[arg(long, default_value_t = tempo_precompiles::PATH_USD_ADDRESS)]
    token: Address,
    /// L1 recipient. Defaults to the signing account.
    #[arg(long)]
    to: Option<Address>,
    /// Private authorization nonce (not an Ethereum transaction nonce). Defaults to random 256 bits.
    #[arg(long)]
    nonce: Option<U256>,
    /// Exclusive L1 admission deadline, in Unix seconds. Defaults to latest L1 timestamp + 600.
    #[arg(long)]
    admit_before: Option<u64>,
    /// Approve exactly the compensation amount if the payer's allowance is insufficient.
    #[arg(long)]
    approve: bool,
    /// Wait for the L1 settled inbox cursor; this does NOT establish payout or rejection reason.
    #[arg(long)]
    wait_for_processing: bool,
    /// Maximum seconds to wait for inbox settlement after admission.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_secs: u64,
}

// The top-level CLI derives Debug; never expose either signing key through it.
impl std::fmt::Debug for ForcedWithdraw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForcedWithdraw").finish_non_exhaustive()
    }
}

fn authorization(
    signer: &PrivateKeySigner,
    recipient: Address,
    token: Address,
    chain_id: u64,
    nonce: U256,
    deadline: Option<u64>,
    now: u64,
) -> eyre::Result<ForcedExitAuthorization> {
    ensure!(!recipient.is_zero(), "recipient must not be zero");
    let admit_before = match deadline {
        Some(value) => value,
        None => now
            .checked_add(600)
            .ok_or_else(|| eyre!("deadline overflow"))?,
    };
    ensure!(
        admit_before > now,
        "admission deadline must be later than the latest L1 timestamp"
    );
    Ok(ForcedExitAuthorization {
        account: signer.address(),
        zoneChainId: U256::from(chain_id),
        token,
        recipient,
        nonce,
        admitBefore: admit_before,
    })
}

impl ForcedWithdraw {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let signer: PrivateKeySigner = self
            .private_key
            .parse()
            .wrap_err("invalid account root key")?;
        let payer: PrivateKeySigner = match &self.fee_payer_private_key {
            Some(key) => key.parse().wrap_err("invalid fee payer key")?,
            None => signer.clone(),
        };
        let payer_address = payer.address();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(EthereumWallet::from(payer))
            .connect(&self.l1_rpc_url)
            .await?;
        let portal = ZonePortal::new(self.portal, &provider);
        ensure!(
            portal.forcedExitVersion().call().await? == 1,
            "forced exits are not active on this portal (expected version 1)"
        );
        ensure!(!portal.paused().call().await?, "portal is paused");
        ensure!(
            portal.tokenConfig(self.token).call().await?.enabled,
            "token is not enabled on portal"
        );
        let l1_chain_id = provider.get_chain_id().await?;
        let chain_id = zone_chain_id(l1_chain_id, portal.zoneId().call().await?)?;
        let header = provider
            .get_header_by_number(alloy_eips::BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("latest L1 header unavailable"))?;
        let mut random_nonce = [0u8; 32];
        OsRng.fill_bytes(&mut random_nonce);
        let auth = authorization(
            &signer,
            self.to.unwrap_or(signer.address()),
            self.token,
            chain_id,
            self.nonce
                .unwrap_or_else(|| U256::from_be_bytes(random_nonce)),
            self.admit_before,
            header.timestamp(),
        )?;
        let compensation = U256::from(portal.FORCED_EXIT_COMPENSATION().call().await?);
        let token = ITIP20::new(self.token, &provider);
        ensure!(
            token.balanceOf(payer_address).call().await? >= compensation,
            "fee payer lacks {compensation} token base units for compensation (transaction gas is additional)"
        );
        let allowance = token.allowance(payer_address, self.portal).call().await?;
        if allowance < compensation {
            ensure!(
                self.approve,
                "insufficient compensation allowance; approve {compensation} for portal {} or pass --approve",
                self.portal
            );
            let receipt = token
                .approve(self.portal, compensation)
                .send_sync()
                .await
                .wrap_err("compensation approval failed")?;
            ensure!(
                receipt.status(),
                "compensation approval reverted: {}",
                receipt.transaction_hash
            );
            println!("Compensation approval: {}", receipt.transaction_hash);
        }
        // Fetch after approval to minimize key-rotation races. All encryption context binds to
        // the actual L1 payer, which may differ from the authorized account.
        let (key, key_index) = portal
            .encryption_key()
            .await
            .wrap_err("no usable portal encryption key")?;
        let signature = signer.sign_hash_sync(&exithatch::authorization_digest(
            &auth,
            U256::from(l1_chain_id),
            self.portal,
        ))?;
        let encrypted = encrypt_request(
            &key.x,
            key.normalized_y_parity()
                .ok_or_else(|| eyre!("invalid encryption key parity"))?,
            &auth,
            &signature.as_bytes(),
            EncryptionContext {
                portal: self.portal,
                sender: payer_address,
                key_index,
            },
            &mut OsRng,
        )
        .ok_or_else(|| eyre!("forced-exit encryption failed"))?;
        // Refuse a stale authorization before spending the non-refundable admission fee.
        let header = provider
            .get_header_by_number(alloy_eips::BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("latest L1 header unavailable"))?;
        ensure!(
            header.timestamp() < auth.admitBefore,
            "admission deadline elapsed before submission"
        );
        println!(
            "Requesting full liquid balance withdrawal; compensation: {compensation} token base units."
        );
        println!(
            "Authorization nonce: {}; admit before: {}",
            auth.nonce, auth.admitBefore
        );
        let pending = portal
            .requestForcedExit(self.token, key_index, encrypted)
            .send()
            .await
            .wrap_err("failed to submit forced withdrawal")?;
        println!("Admission transaction: {}", pending.tx_hash());
        let receipt = pending.get_receipt().await.wrap_err(
            "failed to confirm admission; inspect the printed transaction before retrying",
        )?;
        ensure!(
            receipt.status(),
            "forced withdrawal admission reverted: {}",
            receipt.transaction_hash
        );
        let event = receipt
            .inner
            .logs()
            .iter()
            .filter(|log| log.address() == self.portal)
            .find_map(|log| ZonePortal::ForcedExitRequested::decode_log(&log.inner).ok())
            .ok_or_else(|| eyre!("successful receipt is missing ForcedExitRequested"))?;
        println!(
            "Admitted request {}; inbox deposit number {}.",
            event.entry.requestId, event.depositNumber
        );
        println!(
            "Admission is not payout confirmation; rejected and empty requests also consume inbox entries."
        );
        if self.wait_for_processing {
            tokio::time::timeout(Duration::from_secs(self.timeout_secs), async {
                loop {
                    if portal.lastProcessedDepositNumber().call().await? >= event.depositNumber {
                        return Ok::<_, eyre::Report>(());
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }).await.map_err(|_| eyre!("timed out waiting for inbox settlement; request {} remains admitted, do not blindly resubmit", event.entry.requestId))??;
            println!(
                "Inbox processing settled on L1. Payout, rejection, or bounce-back must be verified separately."
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::from_slice(&[7; 32]).unwrap()
    }

    #[test]
    fn authorization_is_accepted_by_execution_and_binds_domain() {
        let signer = signer();
        let portal = Address::repeat_byte(3);
        let token = tempo_precompiles::PATH_USD_ADDRESS;
        let chain = zone_chain_id(1337, 42).unwrap();
        let auth = authorization(
            &signer,
            Address::repeat_byte(4),
            token,
            chain,
            U256::from(99),
            None,
            100,
        )
        .unwrap();
        assert_eq!(auth.admitBefore, 700);
        let sig = signer
            .sign_hash_sync(&exithatch::authorization_digest(
                &auth,
                U256::from(1337),
                portal,
            ))
            .unwrap()
            .as_bytes();
        let payload = exithatch::encode_payload(&auth, &sig);
        let (decoded, decoded_sig) = exithatch::decode_payload(&payload).unwrap();
        assert!(
            exithatch::authorize(
                &decoded,
                &decoded_sig,
                U256::from(1337),
                portal,
                U256::from(chain),
                token,
                699
            )
            .is_ok()
        );
        for (parent, target, timestamp) in [
            (1338, portal, 699),
            (1337, Address::ZERO, 699),
            (1337, portal, 700),
        ] {
            assert!(
                exithatch::authorize(
                    &decoded,
                    &decoded_sig,
                    U256::from(parent),
                    target,
                    U256::from(chain),
                    token,
                    timestamp
                )
                .is_err()
            );
        }
    }

    #[test]
    fn rejects_invalid_recipient_and_deadlines_before_submission() {
        let signer = signer();
        for (to, deadline, now) in [
            (Address::ZERO, Some(101), 100),
            (signer.address(), Some(100), 100),
            (signer.address(), Some(99), 100),
            (signer.address(), None, u64::MAX),
        ] {
            assert!(
                authorization(
                    &signer,
                    to,
                    tempo_precompiles::PATH_USD_ADDRESS,
                    123,
                    U256::ZERO,
                    deadline,
                    now
                )
                .is_err()
            );
        }
    }

    #[test]
    fn cli_needs_no_zone_rpc_and_redacts_both_keys() {
        let root = "07".repeat(32);
        let payer = "08".repeat(32);
        let args = [
            "forced-withdraw",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--portal",
            "0x1111111111111111111111111111111111111111",
            "--private-key",
            &root,
            "--fee-payer-private-key",
            &payer,
            "--nonce",
            "42",
            "--approve",
            "--wait-for-processing",
        ];
        let command = ForcedWithdraw::try_parse_from(args).unwrap();
        assert_eq!(command.nonce, Some(U256::from(42)));
        assert!(command.approve && command.wait_for_processing);
        let debug = format!("{command:?}");
        assert!(!debug.contains(&root) && !debug.contains(&payer));
        assert!(
            ForcedWithdraw::try_parse_from(args.into_iter().chain(["--timeout-secs", "0"]))
                .is_err()
        );
        assert!(ForcedWithdraw::try_parse_from(args.into_iter().chain(["--amount", "1"])).is_err());
    }
}
