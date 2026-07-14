use std::{collections::BTreeSet, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{
    AddressableManager as _, Receiver as _, Recipients, Sender as _, authenticated::lookup,
};
use commonware_runtime::{Runner as _, Spawner as _};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    P2pNetworkId, Role, ZoneManifest,
    identity::Ed25519Identity,
    messages::ControlMessage,
    network::{self, CONTROL_BACKLOG, CONTROL_CHANNEL},
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_BACKLOG: usize = 128;
const EVENT_BACKLOG: usize = 128;

/// Fully validated configuration for one node's Zone P2P runtime.
#[derive(Clone)]
pub struct P2pConfig {
    manifest: Arc<ZoneManifest>,
    ed25519_identity: Ed25519Identity,
    listen: SocketAddr,
    role: Role,
}

impl P2pConfig {
    /// Loads the Commonware Ed25519 key and manifest, then validates this node's
    /// membership, zone ID, and optional role assertion.
    pub fn load(
        manifest_path: impl AsRef<Path>,
        ed25519_key_path: impl AsRef<Path>,
        listen: SocketAddr,
        expected_zone_id: u32,
        asserted_role: Option<Role>,
    ) -> eyre::Result<Self> {
        let ed25519_identity = Ed25519Identity::read_from_file(ed25519_key_path)?;
        let manifest = ZoneManifest::read_from_file(manifest_path)?;
        let role = manifest.validate_node(
            expected_zone_id,
            &ed25519_identity.ed25519_public_key(),
            asserted_role,
        )?;
        Ok(Self {
            manifest: Arc::new(manifest),
            ed25519_identity,
            listen,
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
            .field("listen", &self.listen)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

/// Outbound protocol commands accepted by the dedicated P2P runtime.
///
/// The Commonware sender remains owned by the dedicated runtime. Callers communicate with it
/// through the bounded Tokio channel exposed by [`P2pHandle`]. These PoC variants will be
/// replaced by the block, ACK/signature, transaction-forwarding, and backfill commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum P2pCommand {
    /// Send a PoC heartbeat to one authenticated peer.
    Heartbeat {
        /// Intended peer's Ed25519 Commonware identity.
        recipient: PublicKey,
        /// Heartbeat nonce.
        nonce: u64,
    },
    /// Acknowledge a PoC heartbeat from one authenticated peer.
    HeartbeatAck {
        /// Intended peer's Ed25519 Commonware identity.
        recipient: PublicKey,
        /// Acknowledged nonce.
        nonce: u64,
    },
}

impl P2pCommand {
    fn into_wire_parts(self) -> (PublicKey, ControlMessage) {
        match self {
            Self::Heartbeat { recipient, nonce } => {
                (recipient, ControlMessage::Heartbeat { nonce })
            }
            Self::HeartbeatAck { recipient, nonce } => {
                (recipient, ControlMessage::HeartbeatAck { nonce })
            }
        }
    }
}

/// Observable lifecycle and heartbeat events emitted by the P2P runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum P2pEvent {
    /// The network and control channel were started.
    Started {
        role: Role,
        ed25519_public_key: PublicKey,
        listen: SocketAddr,
    },
    /// A leader received a heartbeat from a follower.
    HeartbeatReceived {
        follower_ed25519_public_key: PublicKey,
        nonce: u64,
    },
    /// A follower received the leader's acknowledgement.
    HeartbeatAcknowledged {
        leader_ed25519_public_key: PublicKey,
        nonce: u64,
    },
}

/// Handle used to communicate with, supervise, and stop the dedicated P2P runtime.
pub struct P2pHandle {
    shutdown: CancellationToken,
    stopped: oneshot::Receiver<Result<(), String>>,
    commands: mpsc::Sender<P2pCommand>,
    events: mpsc::Receiver<P2pEvent>,
}

/// Cross-runtime channels and lifecycle controls returned by [`P2pHandle::into_parts`].
pub struct P2pHandleParts {
    /// Cancels the dedicated P2P runtime.
    pub shutdown: CancellationToken,
    /// Resolves when the dedicated P2P runtime exits.
    pub stopped: oneshot::Receiver<Result<(), String>>,
    /// Bounded outbound command channel into the dedicated P2P runtime.
    pub commands: mpsc::Sender<P2pCommand>,
    /// Bounded inbound event channel from the dedicated P2P runtime.
    pub events: mpsc::Receiver<P2pEvent>,
}

impl P2pHandle {
    /// Splits the handle into the pieces needed by a node supervisor or test.
    pub fn into_parts(self) -> P2pHandleParts {
        P2pHandleParts {
            shutdown: self.shutdown,
            stopped: self.stopped,
            commands: self.commands,
            events: self.events,
        }
    }
}

/// Starts Commonware and the role-specific PoC heartbeat actor on a dedicated OS thread.
pub fn spawn_p2p(config: P2pConfig, network_id: P2pNetworkId) -> eyre::Result<P2pHandle> {
    let shutdown = CancellationToken::new();
    let thread_shutdown = shutdown.clone();
    let (stopped_tx, stopped) = oneshot::channel();
    let (commands, command_rx) = mpsc::channel(COMMAND_BACKLOG);
    let runtime_commands = commands.clone();
    let (events_tx, events) = mpsc::channel(EVENT_BACKLOG);

    std::thread::Builder::new()
        .name(format!("zone-p2p-{}", config.role()))
        .spawn(move || {
            let result = run(
                config,
                network_id,
                thread_shutdown,
                runtime_commands,
                command_rx,
                events_tx,
            )
            .map_err(|err| format!("{err:?}"));
            let _ = stopped_tx.send(result);
        })
        .map_err(|err| eyre::eyre!("failed spawning P2P runtime thread: {err}"))?;

    Ok(P2pHandle {
        shutdown,
        stopped,
        commands,
        events,
    })
}

fn run(
    config: P2pConfig,
    network_id: P2pNetworkId,
    shutdown: CancellationToken,
    commands: mpsc::Sender<P2pCommand>,
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
            network_id,
        )?;
        oracle.track(0, peers).await;
        let (sender, receiver) = commonware.register(
            CONTROL_CHANNEL,
            network::control_quota(),
            CONTROL_BACKLOG,
        );
        let mut network_task = commonware.start();

        if config.manifest.has_dns_addresses() {
            warn!(
                target: "zone::p2p",
                "DNS peer addresses configured; relying on manifest Ed25519 public keys instead of source-IP filtering"
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

        let command_loop = run_commands(sender, command_rx);
        tokio::pin!(command_loop);

        let heartbeat = run_heartbeat(
            config.role,
            config.manifest,
            commands,
            receiver,
            events,
        );
        tokio::pin!(heartbeat);

        let result = tokio::select! {
            () = shutdown.cancelled() => Ok(()),
            network_result = &mut network_task => match network_result {
                Ok(()) => Err(eyre::eyre!("Commonware network stopped unexpectedly")),
                Err(err) => Err(eyre::eyre!("Commonware network failed: {err}")),
            },
            result = &mut command_loop => result,
            result = &mut heartbeat => result,
        };

        context
            .stop(0, Some(SHUTDOWN_TIMEOUT))
            .await
            .map_err(|err| eyre::eyre!("failed stopping Commonware runtime: {err}"))?;
        result
    })
}

async fn run_commands(
    mut sender: lookup::Sender<PublicKey, commonware_runtime::tokio::Context>,
    mut commands: mpsc::Receiver<P2pCommand>,
) -> eyre::Result<()> {
    while let Some(command) = commands.recv().await {
        let (recipient, message) = command.into_wire_parts();
        let sent = sender
            .send(Recipients::One(recipient.clone()), message.encode(), true)
            .await
            .map_err(|err| eyre::eyre!("failed sending P2P control message: {err}"))?;
        if sent.is_empty() {
            debug!(target: "zone::p2p", %recipient, ?message, "Peer is not connected; control message was not sent");
        }
    }

    Err(eyre::eyre!("P2P command channel closed unexpectedly"))
}

async fn run_heartbeat(
    role: Role,
    manifest: Arc<ZoneManifest>,
    commands: mpsc::Sender<P2pCommand>,
    receiver: lookup::Receiver<PublicKey>,
    events: mpsc::Sender<P2pEvent>,
) -> eyre::Result<()> {
    match role {
        Role::Leader => run_leader_heartbeat(manifest, commands, receiver, events).await,
        Role::Follower => run_follower_heartbeat(manifest, commands, receiver, events).await,
    }
}

async fn run_leader_heartbeat(
    manifest: Arc<ZoneManifest>,
    commands: mpsc::Sender<P2pCommand>,
    mut receiver: lookup::Receiver<PublicKey>,
    events: mpsc::Sender<P2pEvent>,
) -> eyre::Result<()> {
    let mut seen = BTreeSet::new();
    loop {
        let (peer, bytes) = receiver
            .recv()
            .await
            .map_err(|err| eyre::eyre!("control channel receive failed: {err}"))?;
        let message = match ControlMessage::decode(bytes) {
            Ok(message) => message,
            Err(err) => {
                warn!(target: "zone::p2p", %peer, %err, "Ignoring invalid control message");
                continue;
            }
        };
        if manifest.role_of(&peer) != Some(Role::Follower) {
            warn!(target: "zone::p2p", %peer, ?message, "Ignoring control message from non-follower");
            continue;
        }

        match message {
            ControlMessage::Heartbeat { nonce } => {
                if seen.insert(peer.clone()) {
                    info!(target: "zone::p2p", follower = %peer, "Received first follower heartbeat");
                } else {
                    debug!(target: "zone::p2p", follower = %peer, nonce, "Received follower heartbeat");
                }
                let _ = events
                    .send(P2pEvent::HeartbeatReceived {
                        follower_ed25519_public_key: peer.clone(),
                        nonce,
                    })
                    .await
                    .ok();
                commands
                    .send(P2pCommand::HeartbeatAck {
                        recipient: peer,
                        nonce,
                    })
                    .await
                    .map_err(|_| eyre::eyre!("P2P command channel closed"))?;
            }
            ControlMessage::HeartbeatAck { nonce } => {
                warn!(target: "zone::p2p", follower = %peer, nonce, "Leader received unexpected heartbeat acknowledgement");
            }
        }
    }
}

async fn run_follower_heartbeat(
    manifest: Arc<ZoneManifest>,
    commands: mpsc::Sender<P2pCommand>,
    mut receiver: lookup::Receiver<PublicKey>,
    events: mpsc::Sender<P2pEvent>,
) -> eyre::Result<()> {
    let leader = manifest.leader_ed25519_public_key().clone();
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_nonce = 0_u64;
    let mut acknowledged = false;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let nonce = next_nonce;
                next_nonce = next_nonce.wrapping_add(1);
                commands
                    .send(P2pCommand::Heartbeat {
                        recipient: leader.clone(),
                        nonce,
                    })
                    .await
                    .map_err(|_| eyre::eyre!("P2P command channel closed"))?;
            }
            received = receiver.recv() => {
                let (peer, bytes) = received
                    .map_err(|err| eyre::eyre!("control channel receive failed: {err}"))?;
                let message = match ControlMessage::decode(bytes) {
                    Ok(message) => message,
                    Err(err) => {
                        warn!(target: "zone::p2p", %peer, %err, "Ignoring invalid control message");
                        continue;
                    }
                };
                if peer != leader {
                    warn!(target: "zone::p2p", %peer, ?message, "Ignoring control message from non-leader");
                    continue;
                }
                match message {
                    ControlMessage::HeartbeatAck { nonce } => {
                        if !acknowledged {
                            acknowledged = true;
                            info!(target: "zone::p2p", %leader, "Established heartbeat exchange with leader");
                        } else {
                            debug!(target: "zone::p2p", %leader, nonce, "Leader acknowledged heartbeat");
                        }
                        let _ = events
                            .send(P2pEvent::HeartbeatAcknowledged {
                                leader_ed25519_public_key: leader.clone(),
                                nonce,
                            })
                            .await;
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        warn!(target: "zone::p2p", %leader, nonce, "Follower received unexpected heartbeat request");
                    }
                }
            }
        }
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

    use super::{P2pConfig, P2pEvent, spawn_p2p};
    use crate::{P2pNetworkId, ZoneManifest, identity::Ed25519Identity};

    fn available_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    fn ed25519_identity(seed: u64) -> Ed25519Identity {
        let key = PrivateKey::from_seed(seed);
        Ed25519Identity::from_hex(&const_hex::encode_prefixed(key.encode().as_ref())).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_nodes_exchange_heartbeats() {
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
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\naddress = \"{address}\"\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref())
            ));
        }
        let manifest = Arc::new(ZoneManifest::parse(&input).unwrap());
        let mut handles = identities
            .into_iter()
            .zip(addresses)
            .map(|(identity, listen)| {
                let role = manifest
                    .validate_node(9, &identity.ed25519_public_key(), None)
                    .unwrap();
                spawn_p2p(
                    P2pConfig {
                        manifest: manifest.clone(),
                        ed25519_identity: identity,
                        listen,
                        role,
                    },
                    P2pNetworkId::new(1, address!("1111111111111111111111111111111111111111")),
                )
                .unwrap()
                .into_parts()
            })
            .collect::<Vec<_>>();

        for handle in handles.iter_mut().skip(1) {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if matches!(
                        handle.events.recv().await,
                        Some(P2pEvent::HeartbeatAcknowledged { .. })
                    ) {
                        break;
                    }
                }
            })
            .await
            .expect("follower did not receive heartbeat acknowledgement");
        }

        for handle in &handles {
            handle.shutdown.cancel();
        }
        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle.stopped)
                .await
                .expect("P2P runtime did not stop")
                .expect("P2P runtime dropped its completion channel")
                .expect("P2P runtime failed");
        }
    }
}
