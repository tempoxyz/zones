use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use alloy_primitives::Address as EthereumAddress;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{
    AddressableManager as _, Receiver as _, Recipients, Sender as _, authenticated::lookup,
};
use commonware_runtime::{Runner as _, Spawner as _};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    P2pNetworkId, Role, ZoneManifest,
    identity::{Ed25519Identity, Secp256k1Identity},
    network::{self, BLOCK_BACKLOG, BLOCK_CHANNEL, MAX_MESSAGE_SIZE},
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const BROADCAST_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const BROADCAST_RETRY_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_BACKLOG: usize = 128;
const EVENT_BACKLOG: usize = 128;

/// Fully validated configuration for one node's Zone P2P runtime.
#[derive(Clone)]
pub struct P2pConfig {
    manifest: Arc<ZoneManifest>,
    ed25519_identity: Ed25519Identity,
    // This individual node key will be used to sign zone blocks for the on-chain quorum.
    secp256k1_identity: Secp256k1Identity,
    listen: SocketAddr,
    bypass_ip_check: bool,
    role: Role,
}

impl P2pConfig {
    /// Loads the Commonware Ed25519 key and manifest, then validates this node's
    /// membership, zone ID, and optional role assertion.
    pub fn load(
        manifest_path: impl AsRef<Path>,
        ed25519_key_path: impl AsRef<Path>,
        secp256k1_key_path: impl AsRef<Path>,
        listen: SocketAddr,
        bypass_ip_check: bool,
        expected_zone_id: u32,
        asserted_role: Option<Role>,
    ) -> eyre::Result<Self> {
        let ed25519_identity = Ed25519Identity::read_from_file(ed25519_key_path)?;
        let secp256k1_identity = Secp256k1Identity::read_from_file(secp256k1_key_path)?;
        let manifest = ZoneManifest::read_from_file(manifest_path)?;
        validate_ip_check_configuration(&manifest, bypass_ip_check)?;
        let role = manifest.validate_node(
            expected_zone_id,
            &ed25519_identity.ed25519_public_key(),
            secp256k1_identity.address(),
            asserted_role,
        )?;
        Ok(Self {
            manifest: Arc::new(manifest),
            ed25519_identity,
            secp256k1_identity,
            listen,
            bypass_ip_check,
            role,
        })
    }

    /// Manifest-derived role for this node.
    pub const fn role(&self) -> Role {
        self.role
    }

    /// This node's Ed25519 public key used by Commonware.
    pub fn ed25519_public_key(&self) -> PublicKey {
        self.ed25519_identity.ed25519_public_key()
    }

    /// This node's address derived from its individual secp256k1 key.
    pub fn secp256k1_address(&self) -> EthereumAddress {
        self.secp256k1_identity.address()
    }

    /// Local socket bound by Commonware.
    pub const fn listen(&self) -> SocketAddr {
        self.listen
    }
}

impl std::fmt::Debug for P2pConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P2pConfig")
            .field("zone_id", &self.manifest.zone_id())
            .field("ed25519_public_key", &self.ed25519_public_key())
            .field("secp256k1_address", &self.secp256k1_address())
            .field("listen", &self.listen)
            .field("bypass_ip_check", &self.bypass_ip_check)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

fn validate_ip_check_configuration(
    manifest: &ZoneManifest,
    bypass_ip_check: bool,
) -> eyre::Result<()> {
    eyre::ensure!(
        !manifest.has_dns_addresses() || bypass_ip_check,
        "DNS peer addresses require --p2p.bypass-ip-check because their egress IPs are not known; this disables pre-authentication source-IP filtering for all inbound P2P connections"
    );
    Ok(())
}

/// Outbound protocol commands accepted by the dedicated P2P runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum P2pCommand {
    /// Broadcast one RLP-encoded sealed zone block to all configured followers.
    BroadcastBlock(Vec<u8>),
}

/// Observable lifecycle and block events emitted by the P2P runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum P2pEvent {
    /// The network and block channel were started.
    Started {
        role: Role,
        ed25519_public_key: PublicKey,
        listen: SocketAddr,
    },
    /// A follower received an encoded sealed block from its configured leader.
    BlockReceived {
        leader_ed25519_public_key: PublicKey,
        block: Vec<u8>,
    },
}

/// Handle used to communicate with, supervise, and stop the dedicated P2P runtime.
pub struct P2pHandle {
    parts: Option<P2pHandleParts>,
}

/// Cross-runtime channels and lifecycle controls returned by [`P2pHandle::into_parts`].
pub struct P2pHandleParts {
    /// Cancels the dedicated P2P runtime.
    pub shutdown: CancellationToken,
    /// Resolves when the dedicated P2P runtime exits.
    pub stopped: oneshot::Receiver<Result<(), String>>,
    /// OS thread hosting the dedicated Commonware runtime.
    pub thread: std::thread::JoinHandle<()>,
    /// Bounded outbound command channel into the dedicated P2P runtime.
    pub commands: mpsc::Sender<P2pCommand>,
    /// Bounded inbound event channel from the dedicated P2P runtime.
    pub events: mpsc::Receiver<P2pEvent>,
}

impl P2pHandle {
    /// Returns the inbound P2P event channel.
    pub fn events_mut(&mut self) -> &mut mpsc::Receiver<P2pEvent> {
        &mut self
            .parts
            .as_mut()
            .expect("P2P handle already consumed")
            .events
    }

    /// Requests shutdown, waits for the Commonware runtime, and joins its OS thread.
    pub async fn shutdown(mut self) -> eyre::Result<()> {
        let P2pHandleParts {
            shutdown,
            stopped,
            thread,
            commands,
            events,
        } = self.parts.take().expect("P2P handle already consumed");
        shutdown.cancel();

        // Close the caller-side channels while the runtime is winding down.
        drop(commands);
        drop(events);
        let stopped_result = stopped.await;

        join_runtime_thread(thread).await?;
        stopped_result
            .map_err(|err| eyre::eyre!("P2P runtime dropped its completion channel: {err}"))?
            .map_err(|err| eyre::eyre!("P2P runtime failed: {err}"))
    }

    /// Splits the handle into the pieces needed by a node supervisor.
    pub fn into_parts(mut self) -> P2pHandleParts {
        self.parts.take().expect("P2P handle already consumed")
    }
}

impl Drop for P2pHandle {
    fn drop(&mut self) {
        if let Some(parts) = &self.parts {
            parts.shutdown.cancel();
        }
    }
}

async fn join_runtime_thread(thread: std::thread::JoinHandle<()>) -> eyre::Result<()> {
    tokio::task::spawn_blocking(move || thread.join())
        .await
        .map_err(|err| eyre::eyre!("failed joining P2P runtime thread: {err}"))?
        .map_err(|_| eyre::eyre!("P2P runtime thread panicked"))
}

/// Starts Commonware block transport on a dedicated OS thread.
pub fn spawn_p2p(config: P2pConfig, network_id: P2pNetworkId) -> eyre::Result<P2pHandle> {
    let shutdown = CancellationToken::new();
    let thread_shutdown = shutdown.clone();
    let (stopped_tx, stopped) = oneshot::channel();
    let (commands, command_rx) = mpsc::channel(COMMAND_BACKLOG);
    let (events_tx, events) = mpsc::channel(EVENT_BACKLOG);

    let thread = std::thread::Builder::new()
        .name(format!("zone-p2p-{}", config.role()))
        .spawn(move || {
            let result = run(config, network_id, thread_shutdown, command_rx, events_tx)
                .map_err(|err| format!("{err:?}"));
            let _ = stopped_tx.send(result);
        })
        .map_err(|err| eyre::eyre!("failed spawning P2P runtime thread: {err}"))?;

    Ok(P2pHandle {
        parts: Some(P2pHandleParts {
            shutdown,
            stopped,
            thread,
            commands,
            events,
        }),
    })
}

fn run(
    config: P2pConfig,
    network_id: P2pNetworkId,
    shutdown: CancellationToken,
    command_rx: mpsc::Receiver<P2pCommand>,
    events: mpsc::Sender<P2pEvent>,
) -> eyre::Result<()> {
    let runtime_config = commonware_runtime::tokio::Config::default()
        .with_tcp_nodelay(Some(true))
        .with_worker_threads(2)
        .with_catch_panics(true);
    commonware_runtime::tokio::Runner::new(runtime_config).start(|context| async move {
        let local_ed25519_public_key = config.ed25519_public_key();
        let (mut commonware, mut oracle, peers) = network::instantiate(
            context.clone(),
            &config.manifest,
            config.ed25519_identity.into_private_key(),
            config.listen,
            config.bypass_ip_check,
            network_id,
        )?;
        oracle.track(0, peers).await;
        let (sender, receiver) =
            commonware.register(BLOCK_CHANNEL, network::block_quota(), BLOCK_BACKLOG);
        let mut network_task = commonware.start();

        if config.bypass_ip_check {
            warn!(
                target: "zone::p2p",
                "P2P source-IP filtering is disabled; relying on network-level access controls and manifest Ed25519 public keys"
            );
        }
        info!(
            target: "zone::p2p",
            zone_id = config.manifest.zone_id(),
            role = %config.role,
            ed25519_public_key = %local_ed25519_public_key,
            listen = %config.listen,
            peers = config.manifest.nodes().len(),
            "Started P2P networking"
        );
        let _ = events
            .send(P2pEvent::Started {
                role: config.role,
                ed25519_public_key: local_ed25519_public_key,
                listen: config.listen,
            })
            .await;

        let followers = config
            .manifest
            .nodes()
            .iter()
            .filter(|node| config.manifest.role_of(node.ed25519_public_key()) == Some(Role::Follower))
            .map(|node| node.ed25519_public_key().clone())
            .collect();
        let command_loop = run_commands(config.role, followers, sender, command_rx);
        tokio::pin!(command_loop);

        let receive_loop = run_block_receiver(
            config.role,
            config.manifest,
            receiver,
            events,
        );
        tokio::pin!(receive_loop);

        let result = tokio::select! {
            biased;
            () = shutdown.cancelled() => Ok(()),
            network_result = &mut network_task => match network_result {
                Ok(()) => Err(eyre::eyre!("Commonware network stopped unexpectedly")),
                Err(err) => Err(eyre::eyre!("Commonware network failed: {err}")),
            },
            result = &mut command_loop => result,
            result = &mut receive_loop => result,
        };

        context
            .stop(0, Some(SHUTDOWN_TIMEOUT))
            .await
            .map_err(|err| eyre::eyre!("failed stopping Commonware runtime: {err}"))?;
        result
    })
}

async fn run_commands(
    role: Role,
    followers: Vec<PublicKey>,
    mut sender: lookup::Sender<PublicKey, commonware_runtime::tokio::Context>,
    mut commands: mpsc::Receiver<P2pCommand>,
) -> eyre::Result<()> {
    while let Some(command) = commands.recv().await {
        if role != Role::Leader {
            warn!(target: "zone::p2p", ?command, "Ignoring outbound block command on follower");
            continue;
        }
        let P2pCommand::BroadcastBlock(block) = command;
        if block.len() > MAX_MESSAGE_SIZE as usize {
            error!(
                target: "zone::p2p",
                block_size_bytes = block.len(),
                max_message_size_bytes = MAX_MESSAGE_SIZE,
                "Canonical block exceeds the P2P message size limit; block was not broadcast"
            );
            continue;
        }
        let sent = tokio::time::timeout(BROADCAST_RETRY_TIMEOUT, async {
            loop {
                let sent = sender
                    .send(Recipients::Some(followers.clone()), block.clone(), true)
                    .await
                    .map_err(|err| eyre::eyre!("failed broadcasting zone block: {err}"))?;
                if !sent.is_empty() || followers.is_empty() {
                    return Ok::<_, eyre::Report>(sent);
                }
                debug!(
                    target: "zone::p2p",
                    "No followers are connected; retrying canonical block broadcast"
                );
                tokio::time::sleep(BROADCAST_RETRY_INTERVAL).await;
            }
        })
        .await;
        let sent = match sent {
            Ok(sent) => sent?,
            Err(_) => {
                warn!(
                    target: "zone::p2p",
                    timeout_secs = BROADCAST_RETRY_TIMEOUT.as_secs(),
                    "No followers connected before block broadcast timed out"
                );
                continue;
            }
        };
        if sent.len() != followers.len() {
            debug!(target: "zone::p2p", connected = sent.len(), configured = followers.len(), "Some followers are not connected; block was not sent to them");
        }
    }

    Err(eyre::eyre!("P2P command channel closed unexpectedly"))
}

async fn run_block_receiver(
    role: Role,
    manifest: Arc<ZoneManifest>,
    mut receiver: lookup::Receiver<PublicKey>,
    events: mpsc::Sender<P2pEvent>,
) -> eyre::Result<()> {
    let leader = manifest.leader_ed25519_public_key().clone();
    loop {
        let (peer, bytes) = receiver
            .recv()
            .await
            .map_err(|err| eyre::eyre!("block channel receive failed: {err}"))?;
        if role == Role::Leader {
            warn!(target: "zone::p2p", %peer, "Leader received an unexpected block message");
            continue;
        }
        if peer != leader {
            warn!(target: "zone::p2p", %peer, "Ignoring block from non-leader");
            continue;
        }
        events
            .send(P2pEvent::BlockReceived {
                leader_ed25519_public_key: peer,
                block: bytes.into(),
            })
            .await
            .map_err(|_| eyre::eyre!("P2P event channel closed"))?;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{SocketAddr, TcpListener},
        sync::Arc,
        time::Duration,
    };

    use alloy_primitives::address;
    use commonware_codec::Encode as _;
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::{P2pCommand, P2pConfig, P2pEvent, spawn_p2p, validate_ip_check_configuration};
    use crate::{
        P2pNetworkId, ZoneManifest,
        identity::{Ed25519Identity, Secp256k1Identity},
        network::MAX_MESSAGE_SIZE,
    };

    fn available_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    fn ed25519_identity(seed: u64) -> Ed25519Identity {
        let key = PrivateKey::from_seed(seed);
        Ed25519Identity::from_hex(&const_hex::encode_prefixed(key.encode().as_ref())).unwrap()
    }

    fn secp256k1_identity(seed: u64) -> Secp256k1Identity {
        Secp256k1Identity::from_hex(&format!("0x{seed:064x}")).unwrap()
    }

    #[test]
    fn dns_manifest_requires_explicit_ip_check_bypass() {
        let identities = [
            ed25519_identity(1),
            ed25519_identity(2),
            ed25519_identity(3),
        ];
        let mut input = format!(
            "zone_id = 9\nleader_ed25519_public_key = \"{}\"\n",
            const_hex::encode_prefixed(identities[0].ed25519_public_key().as_ref())
        );
        for (index, identity) in identities.iter().enumerate() {
            let secp256k1_identity = secp256k1_identity(index as u64 + 1);
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"{}\"\naddress = \"node-{index}.zone.local:9200\"\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref()),
                secp256k1_identity.address(),
            ));
        }
        let manifest = ZoneManifest::parse(&input).unwrap();

        let error = validate_ip_check_configuration(&manifest, false).unwrap_err();
        assert!(error.to_string().contains("--p2p.bypass-ip-check"));
        validate_ip_check_configuration(&manifest, true).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leader_broadcasts_blocks_to_followers() {
        let addresses = [
            available_address(),
            available_address(),
            available_address(),
        ];
        let identities = [
            ed25519_identity(1),
            ed25519_identity(2),
            ed25519_identity(3),
        ];
        let mut input = format!(
            "zone_id = 9\nleader_ed25519_public_key = \"{}\"\n",
            const_hex::encode_prefixed(identities[0].ed25519_public_key().as_ref())
        );
        for (index, (identity, address)) in identities.iter().zip(addresses).enumerate() {
            let secp256k1_identity = secp256k1_identity(index as u64 + 1);
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"{}\"\naddress = \"{address}\"\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref()),
                secp256k1_identity.address(),
            ));
        }
        let manifest = Arc::new(ZoneManifest::parse(&input).unwrap());
        let mut handles = identities
            .into_iter()
            .zip(addresses)
            .enumerate()
            .map(|(index, (identity, listen))| {
                let secp256k1_identity = secp256k1_identity(index as u64 + 1);
                let role = manifest
                    .validate_node(
                        9,
                        &identity.ed25519_public_key(),
                        secp256k1_identity.address(),
                        None,
                    )
                    .unwrap();
                spawn_p2p(
                    P2pConfig {
                        manifest: manifest.clone(),
                        ed25519_identity: identity,
                        secp256k1_identity,
                        listen,
                        bypass_ip_check: false,
                        role,
                    },
                    P2pNetworkId::new(1, address!("1111111111111111111111111111111111111111")),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();

        let block = vec![0xf8, 0x01, 0x80];
        let commands = handles[0].parts.as_ref().unwrap().commands.clone();
        let oversized_block = vec![0; MAX_MESSAGE_SIZE as usize + 1];
        commands
            .send(P2pCommand::BroadcastBlock(oversized_block))
            .await
            .expect("P2P command channel should remain open");
        let broadcast_block = block.clone();
        let broadcaster = tokio::spawn(async move {
            loop {
                commands
                    .send(P2pCommand::BroadcastBlock(broadcast_block.clone()))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        for handle in handles.iter_mut().skip(1) {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(P2pEvent::BlockReceived {
                        block: received, ..
                    }) = handle.events_mut().recv().await
                    {
                        assert_eq!(received, block);
                        return;
                    }
                }
            })
            .await
            .expect("follower did not receive block");
        }
        broadcaster.abort();

        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
                .await
                .expect("P2P runtime did not stop")
                .expect("P2P runtime failed");
        }
    }
}
