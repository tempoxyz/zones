//! Configures non-zero bridge fee rates for a production-shaped benchmark deployment.
//!
//! A freshly created portal and outbox default their rates to zero. Deposit and bounce-back rates
//! can be initialized immediately on Tempo L1. The outbox rate is optional because the sequencer
//! needs Zone fee-token balance before it can submit the Zone transaction.

use alloy::{
    network::{EthereumWallet, ReceiptResponse as _},
    primitives::Address,
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
};
use eyre::{WrapErr as _, ensure};
use tempo_alloy::{TempoNetwork, rpc::TempoCallBuilderExt as _};
use tempo_zone_contracts::{ZONE_OUTBOX_ADDRESS, ZoneOutbox, ZonePortal};

const DEFAULT_ZONE_GAS_RATE: u128 = 1;
const DEFAULT_BOUNCEBACK_GAS: u64 = 300_000;

#[derive(Debug, clap::Parser)]
pub(crate) struct ConfigureBenchmarkFees {
    /// Tempo L1 HTTP RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// ZonePortal address on Tempo L1.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Address,

    /// Enabled TIP-20 used to pay all configuration transaction fees.
    #[arg(long, env = "ZONES_BENCH_TOKEN")]
    token: Address,

    /// Zone token units charged per unit of deposit gas.
    #[arg(long, default_value_t = DEFAULT_ZONE_GAS_RATE)]
    zone_gas_rate: u128,

    /// Tempo gas reserved for a failed-deposit bounce-back.
    #[arg(long, default_value_t = DEFAULT_BOUNCEBACK_GAS)]
    bounceback_gas: u64,

    /// Optional trusted Zone HTTP RPC. Must be paired with --tempo-gas-rate.
    #[arg(long, env = "ZONE_RPC_URL", requires = "tempo_gas_rate")]
    zone_rpc_url: Option<String>,

    /// Optional Zone-token units charged per unit of Tempo withdrawal gas.
    /// The sequencer must already hold enough enabled Zone fee token to submit this transaction.
    #[arg(long, requires = "zone_rpc_url")]
    tempo_gas_rate: Option<u128>,

    /// Gas limit for the optional ZoneOutbox fee-rate transaction.
    #[arg(long, default_value_t = 2_000_000)]
    zone_tx_gas_limit: u64,
}

impl ConfigureBenchmarkFees {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        ensure!(
            self.zone_gas_rate > 0,
            "--zone-gas-rate must be greater than zero for a benchmark deployment"
        );
        ensure!(
            self.bounceback_gas > 0,
            "--bounceback-gas must be greater than zero for a benchmark deployment"
        );
        if let Some(tempo_gas_rate) = self.tempo_gas_rate {
            ensure!(
                tempo_gas_rate > 0,
                "--tempo-gas-rate must be greater than zero for a benchmark deployment"
            );
            ensure!(
                self.zone_tx_gas_limit > 0,
                "--zone-tx-gas-limit must be greater than zero"
            );
        }

        // Intentionally read the raw key directly from the environment. There is no clap option
        // for it, so it cannot be placed in the process argument list by this command.
        let key = std::env::var("SEQUENCER_KEY")
            .wrap_err("SEQUENCER_KEY must be set in the environment")?;
        let signer: PrivateKeySigner = key
            .strip_prefix("0x")
            .unwrap_or(&key)
            .parse()
            .wrap_err("SEQUENCER_KEY is not a valid private key")?;
        let sequencer = signer.address();

        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(EthereumWallet::from(signer.clone()))
            .connect(&self.l1_rpc_url)
            .await
            .wrap_err("failed connecting to Tempo L1 RPC")?;
        let portal = ZonePortal::new(self.portal, &l1);
        let onchain_sequencer = portal
            .sequencer()
            .call()
            .await
            .wrap_err("failed querying ZonePortal sequencer")?;
        ensure!(
            onchain_sequencer == sequencer,
            "SEQUENCER_KEY resolves to {sequencer}, but portal {} expects {onchain_sequencer}",
            self.portal
        );

        let current_zone_gas_rate = portal
            .zoneGasRate()
            .call()
            .await
            .wrap_err("failed querying ZonePortal zoneGasRate")?;
        if current_zone_gas_rate != self.zone_gas_rate {
            let receipt = portal
                .setZoneGasRate(self.zone_gas_rate)
                .fee_token(self.token)
                .send()
                .await
                .wrap_err("failed sending ZonePortal.setZoneGasRate")?
                .get_receipt()
                .await
                .wrap_err("failed waiting for ZonePortal.setZoneGasRate receipt")?;
            ensure!(receipt.status(), "ZonePortal.setZoneGasRate reverted");
        }

        let current_bounceback_gas = portal
            .bouncebackGas()
            .call()
            .await
            .wrap_err("failed querying ZonePortal bouncebackGas")?;
        if current_bounceback_gas != self.bounceback_gas {
            let receipt = portal
                .setBouncebackGas(self.bounceback_gas)
                .fee_token(self.token)
                .send()
                .await
                .wrap_err("failed sending ZonePortal.setBouncebackGas")?
                .get_receipt()
                .await
                .wrap_err("failed waiting for ZonePortal.setBouncebackGas receipt")?;
            ensure!(receipt.status(), "ZonePortal.setBouncebackGas reverted");
        }

        ensure!(
            portal.zoneGasRate().call().await? == self.zone_gas_rate,
            "ZonePortal zoneGasRate did not update to {}",
            self.zone_gas_rate
        );
        ensure!(
            portal.bouncebackGas().call().await? == self.bounceback_gas,
            "ZonePortal bouncebackGas did not update to {}",
            self.bounceback_gas
        );

        println!("Configured ZonePortal {}", self.portal);
        println!("  Zone gas rate:  {}", self.zone_gas_rate);
        println!("  Bounceback gas: {}", self.bounceback_gas);

        if let (Some(zone_rpc_url), Some(tempo_gas_rate)) = (self.zone_rpc_url, self.tempo_gas_rate)
        {
            let zone = ProviderBuilder::new_with_network::<TempoNetwork>()
                .wallet(EthereumWallet::from(signer))
                .connect(&zone_rpc_url)
                .await
                .wrap_err("failed connecting to Zone RPC")?;
            let outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, &zone);
            let current_tempo_gas_rate = outbox
                .tempoGasRate()
                .call()
                .await
                .wrap_err("failed querying ZoneOutbox tempoGasRate")?;
            if current_tempo_gas_rate != tempo_gas_rate {
                let receipt = outbox
                    .setTempoGasRate(tempo_gas_rate)
                    .fee_token(self.token)
                    .gas(self.zone_tx_gas_limit)
                    .send()
                    .await
                    .wrap_err(
                        "failed sending ZoneOutbox.setTempoGasRate; confirm the sequencer has Zone fee-token balance",
                    )?
                    .get_receipt()
                    .await
                    .wrap_err("failed waiting for ZoneOutbox.setTempoGasRate receipt")?;
                ensure!(receipt.status(), "ZoneOutbox.setTempoGasRate reverted");
            }
            ensure!(
                outbox.tempoGasRate().call().await? == tempo_gas_rate,
                "ZoneOutbox tempoGasRate did not update to {tempo_gas_rate}"
            );
            println!("Configured ZoneOutbox {ZONE_OUTBOX_ADDRESS}");
            println!("  Tempo gas rate: {tempo_gas_rate}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn required_args() -> [&'static str; 7] {
        [
            "configure-benchmark-fees",
            "--l1-rpc-url",
            "http://127.0.0.1:8545",
            "--portal",
            "0x0000000000000000000000000000000000000001",
            "--token",
            "0x20c0000000000000000000000000000000000000",
        ]
    }

    #[test]
    fn defaults_initialize_nonzero_deposit_and_bounceback_fees() {
        let command = ConfigureBenchmarkFees::try_parse_from(required_args()).unwrap();
        assert_eq!(command.zone_gas_rate, 1);
        assert_eq!(command.bounceback_gas, 300_000);
        assert!(command.zone_rpc_url.is_none());
        assert!(command.tempo_gas_rate.is_none());
        assert_eq!(command.zone_tx_gas_limit, 2_000_000);
    }

    #[test]
    fn zone_rpc_and_tempo_rate_must_be_configured_together() {
        let missing_rate = ConfigureBenchmarkFees::try_parse_from(
            required_args()
                .into_iter()
                .chain(["--zone-rpc-url", "http://127.0.0.1:8546"]),
        )
        .unwrap_err();
        assert_eq!(
            missing_rate.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let missing_rpc = ConfigureBenchmarkFees::try_parse_from(
            required_args().into_iter().chain(["--tempo-gas-rate", "1"]),
        )
        .unwrap_err();
        assert_eq!(
            missing_rpc.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn sequencer_key_has_no_command_line_option() {
        let error = ConfigureBenchmarkFees::try_parse_from(
            required_args()
                .into_iter()
                .chain(["--sequencer-key", "0x01"]),
        )
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
