//! Remote prover routing by the hardfork active on the settlement chain.

use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::BlockHeader as _;
use alloy_provider::{DynProvider, Provider as _};
use alloy_rpc_types_eth::BlockNumberOrTag;
use eyre::{Context as _, Result, ensure};
use reth_metrics::metrics::Gauge;
use tempo_alloy::TempoNetwork;
use tempo_chainspec::{TempoHardforks, hardfork::TempoHardfork};
use zone_chainspec::ZoneChainSpec;

/// Startup and monitoring require endpoints for forks activating within 72 hours, inclusive.
const PROVER_UPGRADE_LOOKAHEAD_SECS: u64 = 72 * 60 * 60;
const PROVER_UPGRADE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

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

    /// Require a prover for the live L1 fork and every later fork scheduled within 72 hours.
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

    /// Observe the schedule every minute, including while the node is idle or a standby.
    /// Run for the node's lifetime, independently of individual prover workers.
    pub async fn monitor_upgrade_readiness(&self, chain_spec: Arc<ZoneChainSpec>) {
        self.monitor_upgrade_readiness_with_gauge(
            chain_spec,
            crate::metrics::ProverMetrics::default().missing_hardfork_prover,
        )
        .await;
    }

    pub(crate) async fn resolve(
        &self,
        provider: &DynProvider<TempoNetwork>,
        chain_spec: &impl TempoHardforks,
    ) -> Result<(&str, TempoHardfork)> {
        let hardfork = active_l1_hardfork(provider, chain_spec).await?;
        Ok((self.address_for(hardfork)?, hardfork))
    }

    fn address_for(&self, hardfork: TempoHardfork) -> Result<&str> {
        self.0
            .get(&hardfork)
            .map(String::as_str)
            .ok_or_else(|| eyre::eyre!("no prover configured for active L1 hardfork {hardfork}"))
    }

    async fn validate_startup_at(
        &self,
        provider: &DynProvider<TempoNetwork>,
        chain_spec: &impl TempoHardforks,
        now: u64,
    ) -> Result<()> {
        let (_, active) = self.resolve(provider, chain_spec).await?;
        if let Some((fork, activation)) = self.missing_upcoming_hardfork(chain_spec, active, now) {
            eyre::bail!(
                "startup requires a prover for L1 hardfork {fork}, scheduled at {activation} within the next 72 hours (or overdue)"
            );
        }
        Ok(())
    }

    fn missing_upcoming_hardfork(
        &self,
        chain_spec: &impl TempoHardforks,
        active: TempoHardfork,
        now: u64,
    ) -> Option<(TempoHardfork, u64)> {
        let deadline = now.saturating_add(PROVER_UPGRADE_LOOKAHEAD_SECS);
        TempoHardfork::VARIANTS.iter().copied().find_map(|fork| {
            let activation = chain_spec.tempo_fork_activation(fork).as_timestamp()?;
            (fork > active && activation <= deadline && !self.0.contains_key(&fork))
                .then_some((fork, activation))
        })
    }

    async fn monitor_upgrade_readiness_with_gauge(
        &self,
        chain_spec: Arc<ZoneChainSpec>,
        gauge: Gauge,
    ) {
        let mut interval = tokio::time::interval(PROVER_UPGRADE_CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(now) => {
                    self.record_upgrade_readiness(chain_spec.as_ref(), now.as_secs(), &gauge)
                }
                Err(error) => {
                    tracing::warn!(%error, "Cannot check upcoming prover hardforks: system clock precedes Unix epoch")
                }
            }
        }
    }

    fn record_upgrade_readiness(&self, chain_spec: &impl TempoHardforks, now: u64, gauge: &Gauge) {
        let active = chain_spec.tempo_hardfork_at(now);
        let missing = !self.0.contains_key(&active)
            || self
                .missing_upcoming_hardfork(chain_spec, active, now)
                .is_some();
        gauge.set(if missing { 1.0 } else { 0.0 });
    }
}

/// Resolve the active L1 fork from its latest block timestamp and the Zone chainspec.
///
/// The local clock can pass an activation while L1 is stalled, so it cannot select a prover.
pub(crate) async fn active_l1_hardfork(
    provider: &DynProvider<TempoNetwork>,
    chain_spec: &impl TempoHardforks,
) -> Result<TempoHardfork> {
    let header = provider
        .get_header_by_number(BlockNumberOrTag::Latest)
        .await
        .wrap_err("failed reading the latest Tempo L1 header")?
        .ok_or_else(|| eyre::eyre!("latest Tempo L1 header is unavailable"))?;
    Ok(chain_spec.tempo_hardfork_at(header.timestamp()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header as ConsensusHeader;
    use alloy_provider::ProviderBuilder;
    use alloy_rpc_types_eth::Header as RpcHeader;
    use alloy_transport::mock::Asserter;
    use tempo_alloy::rpc::TempoHeaderResponse;
    use tempo_primitives::TempoHeader;

    fn mock_l1_header(timestamp: u64) -> serde_json::Value {
        let header = TempoHeaderResponse {
            inner: RpcHeader {
                hash: Default::default(),
                inner: TempoHeader {
                    inner: ConsensusHeader {
                        timestamp,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                total_difficulty: None,
                size: None,
            },
            timestamp_millis: 0,
        };
        serde_json::to_value(header).unwrap()
    }

    #[tokio::test]
    async fn active_fork_uses_latest_l1_block_timestamp() {
        let asserter = Asserter::new();
        asserter.push_success(&mock_l1_header(999));
        asserter.push_success(&mock_l1_header(1_000));
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased();
        assert_eq!(
            active_l1_hardfork(&provider, &scheduled_t13(1_000))
                .await
                .unwrap(),
            TempoHardfork::T12
        );
        assert_eq!(
            active_l1_hardfork(&provider, &scheduled_t13(1_000))
                .await
                .unwrap(),
            TempoHardfork::T13
        );
    }

    #[derive(Default)]
    struct RecordedGauge(std::sync::Mutex<Vec<f64>>);

    impl reth_metrics::metrics::GaugeFn for RecordedGauge {
        fn increment(&self, _: f64) {
            panic!("readiness must set the gauge, not increment it");
        }

        fn decrement(&self, _: f64) {
            panic!("readiness must set the gauge, not decrement it");
        }

        fn set(&self, value: f64) {
            self.0.lock().unwrap().push(value);
        }
    }

    #[test]
    fn metric_detects_entry_into_the_window_and_remains_set_after_activation() {
        let activation = 400_000;
        let spec = scheduled_t13(activation);
        let addresses = ProverAddresses::new(vec!["T12=old:5000".parse().unwrap()])
            .unwrap()
            .unwrap();
        let recorded = Arc::new(RecordedGauge::default());
        let gauge = Gauge::from_arc(recorded.clone());
        for now in [
            activation - PROVER_UPGRADE_LOOKAHEAD_SECS - 1,
            activation - PROVER_UPGRADE_LOOKAHEAD_SECS,
            activation - 1,
            activation,
        ] {
            addresses.record_upgrade_readiness(&spec, now, &gauge);
        }
        routed().record_upgrade_readiness(&spec, activation, &gauge);
        assert_eq!(*recorded.0.lock().unwrap(), vec![0.0, 1.0, 1.0, 1.0, 0.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn readiness_is_refreshed_every_minute_without_prover_jobs() {
        let addresses = routed();
        let chain_spec = Arc::new(ZoneChainSpec {
            inner: Arc::new(scheduled_t13(u64::MAX)),
        });
        let recorded = Arc::new(RecordedGauge::default());
        let gauge = Gauge::from_arc(recorded.clone());
        let task = tokio::spawn(async move {
            addresses
                .monitor_upgrade_readiness_with_gauge(chain_spec, gauge)
                .await;
        });
        tokio::task::yield_now().await;
        assert_eq!(*recorded.0.lock().unwrap(), vec![0.0]);
        tokio::time::advance(Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert_eq!(recorded.0.lock().unwrap().len(), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(*recorded.0.lock().unwrap(), vec![0.0, 0.0]);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

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
    async fn startup_requires_upcoming_forks_including_the_72_hour_boundary() {
        let now = 100_000;
        for activation in [
            now - 1,
            now,
            now + 1,
            now + 48 * 60 * 60,
            now + 72 * 60 * 60,
        ] {
            let asserter = Asserter::new();
            asserter.push_success(&mock_l1_header(0));
            asserter.push_success(&mock_l1_header(0));
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
        asserter.push_success(&mock_l1_header(0));
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
    async fn startup_allows_forks_beyond_72_hours() {
        let asserter = Asserter::new();
        asserter.push_success(&mock_l1_header(0));
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased();
        let config = ProverAddresses::new(vec!["T12=current:5000".parse().unwrap()])
            .unwrap()
            .unwrap();
        config
            .validate_startup_at(
                &provider,
                &scheduled_t13(100_000 + 72 * 60 * 60 + 1),
                100_000,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_fork_and_rpc_failure_never_fall_back() {
        let asserter = Asserter::new();
        asserter.push_success(&mock_l1_header(100_001));
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
        assert!(
            config
                .resolve(&provider, &scheduled_t13(100_001))
                .await
                .is_err()
        );
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn routing_follows_activation_and_reorg_without_caching() {
        let asserter = Asserter::new();
        for timestamp in [999, 1_000, 999] {
            asserter.push_success(&mock_l1_header(timestamp));
        }
        asserter.push_failure_msg("L1 unavailable");
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased();
        let config = routed();
        let spec = scheduled_t13(1_000);
        assert_eq!(
            config.resolve(&provider, &spec).await.unwrap(),
            ("old:5000", TempoHardfork::T12)
        );
        assert_eq!(
            config.resolve(&provider, &spec).await.unwrap(),
            ("new:5000", TempoHardfork::T13)
        );
        assert_eq!(
            config.resolve(&provider, &spec).await.unwrap(),
            ("old:5000", TempoHardfork::T12)
        );
        assert!(config.resolve(&provider, &spec).await.is_err());
    }
}
