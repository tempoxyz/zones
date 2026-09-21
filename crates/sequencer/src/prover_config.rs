//! Remote prover routing by the hardfork active on the settlement chain.

use std::{
    collections::BTreeMap,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_provider::{DynProvider, Provider as _};
use eyre::{Context as _, Result, ensure};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::{TempoHardforks, hardfork::TempoHardfork};

/// Startup requires endpoints for forks activating within one day, inclusive.
const PROVER_UPGRADE_LOOKAHEAD_SECS: u64 = 24 * 60 * 60;

/// One explicit hardfork-to-prover assignment, written as `T13=HOST:PORT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardforkProverAddress {
    /// L1 hardfork for which this prover is approved.
    pub hardfork: TempoHardfork,
    /// TCP endpoint of the immutable prover release.
    pub address: String,
}

impl FromStr for HardforkProverAddress {
    type Err = eyre::Report;

    fn from_str(value: &str) -> Result<Self> {
        let (fork, address) = value.split_once('=').ok_or_else(|| {
            eyre::eyre!("expected prover assignment HARDFORK=HOST:PORT, got {value:?}")
        })?;
        ensure!(
            !address.trim().is_empty(),
            "prover address must not be empty"
        );
        Ok(Self {
            hardfork: fork
                .parse()
                .map_err(|_| eyre::eyre!("unknown Tempo hardfork {fork:?}"))?,
            address: address.to_owned(),
        })
    }
}

/// Remote prover configuration. Fork routing requires an exact assignment, with no fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProverAddresses(BTreeMap<TempoHardfork, String>);

impl ProverAddresses {
    /// Build a configuration, rejecting duplicate assignments.
    pub fn new(assignments: Vec<HardforkProverAddress>) -> Result<Option<Self>> {
        if assignments.is_empty() {
            return Ok(None);
        }
        let mut addresses = BTreeMap::new();
        for assignment in assignments {
            ensure!(
                !assignment.address.trim().is_empty(),
                "prover address must not be empty"
            );
            ensure!(
                addresses
                    .insert(assignment.hardfork, assignment.address)
                    .is_none(),
                "duplicate prover address for {}",
                assignment.hardfork
            );
        }
        Ok(Some(Self(addresses)))
    }

    /// Require a prover for the live L1 fork and every later fork scheduled within 24 hours.
    /// Uses the node's chainspec and wall-clock time, including overdue forks on a lagging L1.
    /// This checks routing configuration, not endpoint connectivity or Nitro measurements.
    pub async fn validate_startup(
        &self,
        provider: &DynProvider<TempoNetwork>,
        chain_spec: &impl TempoHardforks,
    ) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        self.validate_startup_at(provider, chain_spec, now).await
    }

    async fn validate_startup_at(
        &self,
        provider: &DynProvider<TempoNetwork>,
        chain_spec: &impl TempoHardforks,
        now: u64,
    ) -> Result<()> {
        let (_, active) = self.resolve(provider).await?;
        let deadline = now.saturating_add(PROVER_UPGRADE_LOOKAHEAD_SECS);
        for &fork in TempoHardfork::VARIANTS {
            if fork <= active {
                continue;
            }
            if let Some(activation) = chain_spec.tempo_fork_activation(fork).as_timestamp()
                && activation <= deadline
            {
                self.address_for(fork).wrap_err_with(|| format!(
                    "startup requires a prover for L1 hardfork {fork}, scheduled at {activation} within the next 24 hours (or overdue)"
                ))?;
            }
        }
        Ok(())
    }

    fn address_for(&self, hardfork: TempoHardfork) -> Result<&str> {
        self.0
            .get(&hardfork)
            .map(String::as_str)
            .ok_or_else(|| eyre::eyre!("no prover configured for active L1 hardfork {hardfork}"))
    }

    pub(crate) async fn resolve(
        &self,
        provider: &DynProvider<TempoNetwork>,
    ) -> Result<(&str, TempoHardfork)> {
        let hardfork = active_l1_hardfork(provider).await?;
        Ok((self.address_for(hardfork)?, hardfork))
    }
}

/// Read the live L1 policy, rejecting unknown forks rather than silently using an older prover.
pub(crate) async fn active_l1_hardfork(
    provider: &DynProvider<TempoNetwork>,
) -> Result<TempoHardfork> {
    #[derive(Debug, serde::Deserialize)]
    struct ForkSchedule {
        active: String,
    }
    let schedule: ForkSchedule = provider
        .raw_request("tempo_forkSchedule".into(), ())
        .await
        .wrap_err("failed reading the live Tempo L1 hardfork")?;
    schedule
        .active
        .parse()
        .map_err(|_| eyre::eyre!("unsupported active L1 hardfork {:?}", schedule.active))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_provider::ProviderBuilder;
    use alloy_transport::mock::Asserter;

    fn scheduled_t13(timestamp: u64) -> tempo_chainspec::TempoChainSpec {
        let mut genesis = tempo_chainspec::spec::DEV.inner.genesis.clone();
        genesis
            .config
            .extra_fields
            .insert_value("t13Time".into(), timestamp)
            .unwrap();
        tempo_chainspec::TempoChainSpec::from_genesis(genesis)
    }

    #[tokio::test]
    async fn startup_requires_upcoming_forks_including_the_24_hour_boundary() {
        let now = 100_000;
        for activation in [now - 1, now, now + 1, now + PROVER_UPGRADE_LOOKAHEAD_SECS] {
            let asserter = Asserter::new();
            asserter.push_success(&serde_json::json!({ "active": "T12" }));
            asserter.push_success(&serde_json::json!({ "active": "T12" }));
            let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
                .connect_mocked_client(asserter)
                .erased();
            let config = ProverAddresses::new(vec!["T12=current:5000".parse().unwrap()])
                .unwrap()
                .unwrap();
            let spec = scheduled_t13(activation);
            let error = config
                .validate_startup_at(&provider, &spec, now)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("startup requires a prover for L1 hardfork T13")
            );
            routed()
                .validate_startup_at(&provider, &spec, now)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn startup_checks_every_fork_in_the_window() {
        let now = 100_000;
        let mut genesis = scheduled_t13(now + 200).inner.genesis.clone();
        genesis
            .config
            .extra_fields
            .insert_value("t12Time".into(), now + 100)
            .unwrap();
        let spec = tempo_chainspec::TempoChainSpec::from_genesis(genesis);
        let asserter = Asserter::new();
        asserter.push_success(&serde_json::json!({ "active": "T11" }));
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased();
        let config = ProverAddresses::new(vec![
            "T11=current:5000".parse().unwrap(),
            "T12=next:5000".parse().unwrap(),
        ])
        .unwrap()
        .unwrap();
        let error = config
            .validate_startup_at(&provider, &spec, now)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("startup requires a prover for L1 hardfork T13")
        );
    }

    fn routed() -> ProverAddresses {
        ProverAddresses::new(vec![
            "T12=old:5000".parse().unwrap(),
            "T13=new:5000".parse().unwrap(),
        ])
        .unwrap()
        .unwrap()
    }

    #[test]
    fn assignments_are_exact_and_unambiguous() {
        let config = routed();
        assert_eq!(config.address_for(TempoHardfork::T12).unwrap(), "old:5000");
        assert_eq!(config.address_for(TempoHardfork::T13).unwrap(), "new:5000");
        assert!(config.address_for(TempoHardfork::T11).is_err());
        assert!(
            ProverAddresses::new(vec!["T13=a:1".parse().unwrap(), "T13=b:2".parse().unwrap()])
                .is_err()
        );
        for invalid in ["T13", "T13=", "T13= ", "unknown=a:1"] {
            assert!(invalid.parse::<HardforkProverAddress>().is_err());
        }
    }

    #[tokio::test]
    async fn startup_allows_forks_beyond_24_hours() {
        let asserter = Asserter::new();
        asserter.push_success(&serde_json::json!({ "active": "T12" }));
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased();
        let config = ProverAddresses::new(vec!["T12=current:5000".parse().unwrap()])
            .unwrap()
            .unwrap();
        config
            .validate_startup_at(
                &provider,
                &scheduled_t13(100_000 + PROVER_UPGRADE_LOOKAHEAD_SECS + 1),
                100_000,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_fork_and_rpc_failure_never_fall_back() {
        let asserter = Asserter::new();
        asserter.push_success(&serde_json::json!({ "active": "T13" }));
        asserter.push_failure_msg("L1 unavailable");
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let config = ProverAddresses::new(vec!["T12=old:5000".parse().unwrap()])
            .unwrap()
            .unwrap();
        assert!(
            config
                .validate_startup_at(&provider, &scheduled_t13(100_001), 100_000)
                .await
                .unwrap_err()
                .to_string()
                .contains("no prover configured for active L1 hardfork T13")
        );
        assert!(config.resolve(&provider).await.is_err());
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn routing_follows_activation_and_reorg_without_caching() {
        let asserter = Asserter::new();
        for active in ["T12", "T13", "T12", "future"] {
            asserter.push_success(&serde_json::json!({ "active": active }));
        }
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased();
        let config = routed();
        assert_eq!(
            config.resolve(&provider).await.unwrap(),
            ("old:5000", TempoHardfork::T12)
        );
        assert_eq!(
            config.resolve(&provider).await.unwrap(),
            ("new:5000", TempoHardfork::T13)
        );
        assert_eq!(
            config.resolve(&provider).await.unwrap(),
            ("old:5000", TempoHardfork::T12)
        );
        assert!(config.resolve(&provider).await.is_err());
    }
}
