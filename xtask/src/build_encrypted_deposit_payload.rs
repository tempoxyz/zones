use alloy::{
    primitives::{Address, B256},
    providers::ProviderBuilder,
};
use eyre::eyre;
use tempo_alloy::TempoNetwork;

use crate::{
    bridge_utils::{
        build_encrypted_deposit_payload_for_zone, payload_json_to_string, resolve_zone_ref,
        signer_address_from_private_key,
    },
    zone_utils::normalize_http_rpc,
};

#[derive(Debug, clap::Parser)]
pub(crate) struct BuildEncryptedDepositPayload {
    /// Target zone reference: name, zone ID, or portal address.
    #[arg(long)]
    target_zone: String,

    /// Recipient address on the target zone. Defaults to the PRIVATE_KEY signer address.
    #[arg(long)]
    recipient: Option<Address>,

    /// Memo bytes32 for the downstream encrypted deposit.
    #[arg(long, default_value_t = B256::ZERO)]
    memo: B256,

    /// Tempo L1 RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// Optional private key used only to derive the default recipient.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: Option<String>,

    /// ZoneFactory contract address used to resolve zone IDs when local metadata is unavailable.
    #[arg(long)]
    zone_factory: Option<Address>,
}

impl BuildEncryptedDepositPayload {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let http_rpc = normalize_http_rpc(&self.l1_rpc_url);
        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&http_rpc)
            .await?;
        let target_zone = resolve_zone_ref(&self.target_zone, &l1, self.zone_factory).await?;
        let recipient = match (self.recipient, self.private_key.as_deref()) {
            (Some(recipient), _) => recipient,
            (None, Some(private_key)) => signer_address_from_private_key(private_key)?,
            (None, None) => {
                return Err(eyre!(
                    "recipient not provided and PRIVATE_KEY is unavailable to derive a default"
                ));
            }
        };

        let payload =
            build_encrypted_deposit_payload_for_zone(&l1, &target_zone, recipient, self.memo)
                .await?;
        println!("{}", payload_json_to_string(&payload)?);
        Ok(())
    }
}
