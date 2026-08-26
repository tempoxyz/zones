//! End-to-end process crash recovery coverage for a three-member P2P cluster.
//!
//! The ordinary P2P fixtures run every node inside this test process. This fixture starts the
//! production CLI in child copies of the integration-test executable so Unix signals terminate
//! exactly one node while the real Tempo L1 and the other Zone nodes remain alive.

use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    net::{SocketAddr, TcpListener},
    os::unix::process::ExitStatusExt as _,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{B256, U256},
    providers::{Provider as _, ProviderBuilder},
};
use alloy_signer_local::PrivateKeySigner;
use commonware_codec::Encode as _;
use commonware_cryptography::{Signer as _, ed25519::PrivateKey as Ed25519PrivateKey};
use k256::SecretKey;
use tempfile::TempDir;
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::ZonePortal;
use tokio::{process::Child, time::timeout};
use zone_primitives::constants::zone_chain_id;

use crate::utils::{L1TestNode, ZoneCreationConfig, build_l1_anchored_genesis, poll_until};

const NODE_ARGS_ENV: &str = "TEMPO_ZONE_PROCESS_CHAOS_NODE_ARGS";
const CHILD_TEST_NAME: &str = "process_chaos_e2e::process_chaos_node";
const NETWORK_TIMEOUT: Duration = Duration::from_secs(90);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const STALL_QUIET_PERIOD: Duration = Duration::from_secs(2);
const L1_BLOCK_TIME: Duration = Duration::from_millis(250);
const BATCH_INTERVAL: u64 = 4;

const LEADER: usize = 0;
const FOLLOWER: usize = 2;

/// Child-process entry point. The parent invokes only this ignored test and supplies the exact
/// production CLI arguments in an environment variable.
#[test]
#[ignore = "started by the process-chaos parent tests"]
fn process_chaos_node() -> eyre::Result<()> {
    let encoded = std::env::var(NODE_ARGS_ENV)
        .map_err(|_| eyre::eyre!("{NODE_ARGS_ENV} is required in the child node process"))?;
    let args: Vec<String> = serde_json::from_str(&encoded)?;
    std::thread::Builder::new()
        .name("process-chaos-zone-node".to_owned())
        // libtest worker threads have a smaller stack than the production binary's main thread.
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            rustls::crypto::aws_lc_rs::default_provider()
                .install_default()
                .map_err(|_| eyre::eyre!("failed to install rustls CryptoProvider"))?;
            zone_node::init_version_metadata();
            zone_node::cli::ZoneCli::parse_from(args).run()
        })?
        .join()
        .map_err(|_| eyre::eyre!("process-chaos Zone node thread panicked"))?
}

#[derive(Clone, Copy, Debug)]
enum ChaosSignal {
    Sigterm,
    Sigkill,
}

impl ChaosSignal {
    const fn number(self) -> i32 {
        match self {
            Self::Sigterm => libc::SIGTERM,
            Self::Sigkill => libc::SIGKILL,
        }
    }
}

struct ProcessNodeConfig {
    args: Vec<String>,
    http_url: url::Url,
    log_path: PathBuf,
}

struct ProcessChaosNode {
    config: ProcessNodeConfig,
    child: Option<Child>,
}

impl ProcessChaosNode {
    async fn start(config: ProcessNodeConfig) -> eyre::Result<Self> {
        let mut node = Self {
            config,
            child: None,
        };
        node.restart().await?;
        Ok(node)
    }

    fn provider(&self) -> alloy::providers::DynProvider<TempoNetwork> {
        ProviderBuilder::new_with_network()
            .connect_http(self.config.http_url.clone())
            .erased()
    }

    async fn restart(&mut self) -> eyre::Result<()> {
        eyre::ensure!(self.child.is_none(), "cannot restart a running node");
        let stdout = append_log(&self.config.log_path)?;
        let stderr = stdout.try_clone()?;
        let encoded_args = serde_json::to_string(&self.config.args)?;
        let child = tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact", CHILD_TEST_NAME, "--ignored", "--nocapture"])
            .env(NODE_ARGS_ENV, encoded_args)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()?;
        self.child = Some(child);

        let provider = self.provider();
        let deadline = tokio::time::Instant::now() + NETWORK_TIMEOUT;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("child was stored before readiness polling")
                .try_wait()?
            {
                self.child = None;
                eyre::bail!(
                    "node exited during startup with {status}; log tail:\n{}",
                    log_tail(&self.config.log_path)
                );
            }
            if provider.get_chain_id().await.is_ok() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                eyre::bail!(
                    "timed out waiting for RPC readiness at {}; log tail:\n{}",
                    self.config.http_url,
                    log_tail(&self.config.log_path)
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(())
    }

    async fn stop(&mut self, signal: ChaosSignal) -> eyre::Result<ExitStatus> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| eyre::eyre!("cannot signal a stopped node"))?;
        let pid = child
            .id()
            .ok_or_else(|| eyre::eyre!("child has no process ID"))?;

        // SAFETY: `pid` comes from the live child handle owned by this object, and the signal is
        // restricted to SIGTERM or SIGKILL. No process-group or wildcard signalling is used.
        let result = unsafe { libc::kill(pid as i32, signal.number()) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let status = timeout(EXIT_TIMEOUT, child.wait())
            .await
            .map_err(|_| eyre::eyre!("node {pid} did not exit after {signal:?}"))??;

        match signal {
            ChaosSignal::Sigterm => eyre::ensure!(
                status.success(),
                "node did not shut down cleanly after SIGTERM: {status}; log tail:\n{}",
                log_tail(&self.config.log_path)
            ),
            ChaosSignal::Sigkill => eyre::ensure!(
                status.signal() == Some(libc::SIGKILL),
                "node did not exit from SIGKILL: {status}"
            ),
        }
        Ok(status)
    }
}

impl Drop for ProcessChaosNode {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

fn append_log(path: &Path) -> eyre::Result<File> {
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

fn log_tail(path: &Path) -> String {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return "<log unavailable>".to_owned();
    };
    let lines: Vec<_> = contents.lines().collect();
    lines[lines.len().saturating_sub(120)..].join("\n")
}

fn available_address() -> eyre::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

struct ProcessChaosCluster {
    nodes: Vec<ProcessChaosNode>,
    l1: L1TestNode,
    portal_address: alloy::primitives::Address,
    _workspace: TempDir,
}

impl ProcessChaosCluster {
    async fn start() -> eyre::Result<Self> {
        let workspace = tempfile::Builder::new()
            .prefix("tempo-zone-process-chaos-")
            .tempdir()?;
        let l1 =
            L1TestNode::start_with(|config| config.dev.block_time = Some(L1_BLOCK_TIME)).await?;

        let identities = [
            Ed25519PrivateKey::from_seed(501),
            Ed25519PrivateKey::from_seed(502),
            Ed25519PrivateKey::from_seed(503),
        ];
        let public_keys = identities.each_ref().map(|key| key.public_key());
        let attestation_keys = [501_u64, 502, 503].map(|key| format!("0x{key:064x}"));
        let attestation_signers = attestation_keys.each_ref().map(|key| {
            key.parse::<PrivateKeySigner>()
                .expect("valid process-chaos attestation key")
        });
        let shared_sequencer_key = format!("0x{:064x}", 599_u64);
        let shared_sequencer_signer = shared_sequencer_key.parse::<PrivateKeySigner>()?;

        let factory = l1.native_zone_factory().await?;
        let portal_address = l1
            .create_zone_with_admin_sequencers_and_config(
                factory,
                l1.admin_address(),
                attestation_signers
                    .iter()
                    .map(PrivateKeySigner::address)
                    .collect(),
                2,
                ZoneCreationConfig::open(),
            )
            .await?;
        for signer in &attestation_signers {
            l1.fund_user(signer.address(), 10_000_000).await?;
        }
        let encryption_key = SecretKey::from(shared_sequencer_signer.credential());
        l1.set_sequencer_encryption_key_with_signer(
            portal_address,
            &encryption_key,
            attestation_signers[0].clone(),
        )
        .await?;

        let portal = ZonePortal::new(portal_address, l1.provider());
        let zone_id = portal.zoneId().call().await?;
        let (mut genesis, _) = build_l1_anchored_genesis(l1.http_url(), portal_address).await?;
        genesis.config.chain_id = zone_chain_id(l1.provider().get_chain_id().await?, zone_id)?;
        let genesis_path = workspace.path().join("genesis.json");
        std::fs::write(&genesis_path, serde_json::to_vec_pretty(&genesis)?)?;
        let sequencer_key_path = workspace.path().join("sequencer.key");
        std::fs::write(&sequencer_key_path, &shared_sequencer_key)?;

        let p2p_addresses = [
            available_address()?,
            available_address()?,
            available_address()?,
        ];
        let manifest_path = workspace.path().join("manifest.toml");
        let mut manifest = format!(
            "zone_id = {zone_id}\nsequencer_set_version = 0\nleader_ed25519_public_key = \"{}\"\n",
            const_hex::encode_prefixed(public_keys[LEADER].as_ref())
        );
        for (index, ((public_key, signer), address)) in public_keys
            .iter()
            .zip(&attestation_signers)
            .zip(p2p_addresses)
            .enumerate()
        {
            manifest.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"{}\"\naddress = \"{address}\"\n",
                const_hex::encode_prefixed(public_key.as_ref()),
                signer.address(),
            ));
        }
        std::fs::write(&manifest_path, manifest)?;

        let mut configs = Vec::with_capacity(3);
        for index in 0..3 {
            let key_path = workspace.path().join(format!("node-{index}.key"));
            std::fs::write(
                &key_path,
                const_hex::encode_prefixed(identities[index].encode().as_ref()),
            )?;
            let attestation_key_path = workspace.path().join(format!("node-{index}-secp256k1.key"));
            std::fs::write(&attestation_key_path, &attestation_keys[index])?;

            let http_address = available_address()?;
            let redacted_address = available_address()?;
            let reth_p2p_address = available_address()?;
            let datadir = workspace.path().join(format!("node-{index}-data"));
            let log_path = workspace.path().join(format!("node-{index}.log"));
            let reth_log_dir = workspace.path().join(format!("node-{index}-reth-logs"));
            let role = if index == LEADER {
                "leader"
            } else {
                "follower"
            };
            let args = vec![
                "tempo-zone".to_owned(),
                "node".to_owned(),
                "--chain".to_owned(),
                genesis_path.display().to_string(),
                "--datadir".to_owned(),
                datadir.display().to_string(),
                "--l1.rpc-url".to_owned(),
                l1.ws_url().to_string(),
                "--l1.portal-address".to_owned(),
                portal_address.to_string(),
                "--http".to_owned(),
                "--http.addr".to_owned(),
                "127.0.0.1".to_owned(),
                "--http.port".to_owned(),
                http_address.port().to_string(),
                "--http.api".to_owned(),
                "all".to_owned(),
                "--port".to_owned(),
                reth_p2p_address.port().to_string(),
                "--redacted-rpc.port".to_owned(),
                redacted_address.port().to_string(),
                "--ipcdisable".to_owned(),
                "--log.file.directory".to_owned(),
                reth_log_dir.display().to_string(),
                "--zone.batch-interval-blocks".to_owned(),
                BATCH_INTERVAL.to_string(),
                "--withdrawal-poll-interval-secs".to_owned(),
                "1".to_owned(),
                "--sequencer.manifest".to_owned(),
                manifest_path.display().to_string(),
                "--p2p.key".to_owned(),
                key_path.display().to_string(),
                "--secp256k1.key".to_owned(),
                attestation_key_path.display().to_string(),
                "--p2p.listen".to_owned(),
                p2p_addresses[index].to_string(),
                "--sequencer.role".to_owned(),
                role.to_owned(),
                "--sequencer-key-file".to_owned(),
                sequencer_key_path.display().to_string(),
            ];
            configs.push(ProcessNodeConfig {
                args,
                http_url: format!("http://{http_address}").parse()?,
                log_path,
            });
        }

        let mut nodes = Vec::with_capacity(3);
        for config in configs {
            nodes.push(ProcessChaosNode::start(config).await?);
        }
        let cluster = Self {
            nodes,
            l1,
            portal_address,
            _workspace: workspace,
        };
        cluster.wait_all_at(1).await?;
        cluster.assert_same_block(1).await?;
        cluster.wait_for_settlement_after(0).await?;
        Ok(cluster)
    }

    async fn synchronized_head(&self) -> eyre::Result<u64> {
        let head = self.nodes[LEADER].provider().get_block_number().await?;
        self.wait_all_at(head).await?;
        self.assert_same_block(head).await?;
        Ok(head)
    }

    async fn wait_all_at(&self, height: u64) -> eyre::Result<()> {
        for (index, node) in self.nodes.iter().enumerate() {
            let provider = node.provider();
            poll_until(
                NETWORK_TIMEOUT,
                POLL_INTERVAL,
                &format!("node {index} to reach zone block {height}"),
                || async {
                    let current = provider.get_block_number().await?;
                    Ok((current >= height).then_some(()))
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn wait_nodes_at(&self, nodes: &[usize], height: u64) -> eyre::Result<()> {
        for &index in nodes {
            let provider = self.nodes[index].provider();
            poll_until(
                NETWORK_TIMEOUT,
                POLL_INTERVAL,
                &format!("node {index} to reach zone block {height}"),
                || async {
                    let current = provider.get_block_number().await?;
                    Ok((current >= height).then_some(()))
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Wait until the selected nodes agree on a head and that head remains unchanged long enough
    /// to rule out blocks that were already in flight when the leader exited.
    async fn wait_for_stable_head(&self, nodes: &[usize]) -> eyre::Result<u64> {
        eyre::ensure!(
            !nodes.is_empty(),
            "stable-head check requires at least one node"
        );
        let deadline = tokio::time::Instant::now() + NETWORK_TIMEOUT;
        let mut observation: Option<(u64, tokio::time::Instant)> = None;

        loop {
            let mut heads = Vec::with_capacity(nodes.len());
            for &index in nodes {
                heads.push(self.nodes[index].provider().get_block_number().await?);
            }

            if heads.iter().all(|height| *height == heads[0]) {
                match observation {
                    Some((height, since)) if height == heads[0] => {
                        if since.elapsed() >= STALL_QUIET_PERIOD {
                            return Ok(height);
                        }
                    }
                    _ => observation = Some((heads[0], tokio::time::Instant::now())),
                }
            } else {
                observation = None;
            }

            eyre::ensure!(
                tokio::time::Instant::now() < deadline,
                "nodes {nodes:?} did not reach a common stable head; last observed heads: {heads:?}"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn assert_same_block(&self, height: u64) -> eyre::Result<B256> {
        let mut expected = None;
        for (index, node) in self.nodes.iter().enumerate() {
            let block = node
                .provider()
                .get_block_by_number(BlockNumberOrTag::Number(height))
                .await?
                .ok_or_else(|| eyre::eyre!("node {index} is missing zone block {height}"))?;
            match expected {
                None => expected = Some(block.header.hash),
                Some(hash) => eyre::ensure!(
                    block.header.hash == hash,
                    "node {index} diverged at zone block {height}: {} != {hash}",
                    block.header.hash
                ),
            }
        }
        expected.ok_or_else(|| eyre::eyre!("process-chaos cluster has no nodes"))
    }

    async fn wait_for_settlement_after(&self, height: u64) -> eyre::Result<u64> {
        let portal = ZonePortal::new(self.portal_address, self.l1.provider());
        let settled: U256 = poll_until(
            NETWORK_TIMEOUT,
            POLL_INTERVAL,
            &format!("a settlement beyond zone block {height}"),
            || async {
                let settled = portal.zoneHeight().call().await?;
                Ok((settled > U256::from(height)).then_some(settled))
            },
        )
        .await?;
        settled
            .try_into()
            .map_err(|_| eyre::eyre!("settled zone height does not fit in u64"))
    }

    /// Read the portal's height and hash from the same L1 block so they describe one settlement.
    async fn settlement_snapshot(&self) -> eyre::Result<(u64, B256)> {
        let portal = ZonePortal::new(self.portal_address, self.l1.provider());
        let l1_block = self.l1.provider().get_block_number().await?;
        let block_id = BlockId::number(l1_block);
        let height = portal
            .zoneHeight()
            .block(block_id)
            .call()
            .await?
            .try_into()?;
        let hash = portal.blockHash().block(block_id).call().await?;
        Ok((height, hash))
    }

    async fn assert_canonical_settlement_history(&self) -> eyre::Result<()> {
        let portal = ZonePortal::new(self.portal_address, self.l1.provider());
        let events = portal.BatchSubmitted_filter().from_block(0).query().await?;
        eyre::ensure!(!events.is_empty(), "no settlement batches were submitted");
        let mut indices = HashSet::new();
        let mut hashes = HashSet::new();
        for (offset, (event, _)) in events.iter().enumerate() {
            eyre::ensure!(
                indices.insert(event.withdrawalBatchIndex),
                "duplicate settlement batch index {}",
                event.withdrawalBatchIndex
            );
            eyre::ensure!(
                hashes.insert(event.nextBlockHash),
                "settlement reused zone block hash {}",
                event.nextBlockHash
            );
            if offset > 0 {
                eyre::ensure!(
                    event.withdrawalBatchIndex == events[offset - 1].0.withdrawalBatchIndex + 1,
                    "settlement batch indices are not contiguous"
                );
            }
        }

        let (settled_height, settled_hash) = self.settlement_snapshot().await?;
        self.wait_all_at(settled_height).await?;
        let canonical_hash = self.assert_same_block(settled_height).await?;
        eyre::ensure!(
            settled_hash == canonical_hash,
            "Portal settled hash is not canonical at zone block {settled_height}"
        );
        Ok(())
    }

    async fn shutdown(&mut self) -> eyre::Result<()> {
        let mut first_error = None;
        for node in &mut self.nodes {
            if node.child.is_some()
                && let Err(error) = node.stop(ChaosSignal::Sigterm).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

async fn run_recovery_case(victim: usize, signal: ChaosSignal) -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let mut cluster = ProcessChaosCluster::start().await?;
    let result = async {
        let baseline = cluster.synchronized_head().await?;
        cluster.nodes[victim].stop(signal).await?;

        let head_boundary = if victim == LEADER {
            // Leadership is explicit, so followers must not elect themselves while A is down.
            // Drain any blocks already broadcast by A, then use the common quiet head as the
            // recovery fence. A settlement beyond it necessarily contains post-restart work.
            cluster.wait_for_stable_head(&[1, 2]).await?
        } else {
            // A and the remaining follower retain a 2-of-3 quorum and must keep settling.
            let settled = cluster.wait_for_settlement_after(baseline).await?;
            cluster.wait_nodes_at(&[LEADER, 1], settled).await?;
            // Fence recovery at the current producer head, not merely the settlement we just
            // observed, so the final settlement covers new work after this phase.
            let healthy_head = cluster.nodes[LEADER].provider().get_block_number().await?;
            cluster.wait_nodes_at(&[1], healthy_head).await?;
            healthy_head
        };

        // A settlement that landed while the victim was stopped must not satisfy recovery.
        // Combine the current portal height with the drained/healthy node head so recovery must
        // advance both observations after the child process is relaunched.
        let (settlement_before_restart, _) = cluster.settlement_snapshot().await?;
        let recovery_boundary = head_boundary.max(settlement_before_restart);

        cluster.nodes[victim].restart().await?;
        let recovered_settlement = cluster.wait_for_settlement_after(recovery_boundary).await?;
        cluster.wait_all_at(recovered_settlement).await?;
        cluster.assert_same_block(recovered_settlement).await?;
        cluster.assert_canonical_settlement_history().await
    }
    .await;
    let cleanup = cluster.shutdown().await;
    result.and(cleanup)
}

macro_rules! process_recovery_test {
    ($name:ident, $victim:expr, $signal:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() -> eyre::Result<()> {
            run_recovery_case($victim, $signal).await
        }
    };
}

process_recovery_test!(
    test_leader_recovers_after_sigkill,
    LEADER,
    ChaosSignal::Sigkill
);
process_recovery_test!(
    test_leader_recovers_after_sigterm,
    LEADER,
    ChaosSignal::Sigterm
);
process_recovery_test!(
    test_follower_recovers_after_sigkill,
    FOLLOWER,
    ChaosSignal::Sigkill
);
process_recovery_test!(
    test_follower_recovers_after_sigterm,
    FOLLOWER,
    ChaosSignal::Sigterm
);
