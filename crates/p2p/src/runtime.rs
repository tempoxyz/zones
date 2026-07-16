use std::{
    collections::HashMap,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{
    AddressableManager as _, Receiver as _, Recipients, Sender as _, authenticated::lookup,
};
use commonware_runtime::{Runner as _, Spawner as _};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    P2pNetworkId, Role, ZoneManifest,
    identity::Ed25519Identity,
    network::{
        self, BACKFILL_BLOCK_CHANNEL, BACKFILL_COMPLETE_CHANNEL, BACKFILL_REQUEST_CHANNEL,
        BLOCK_BACKLOG, BLOCK_CHANNEL, MAX_MESSAGE_SIZE,
    },
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const BROADCAST_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const BROADCAST_RETRY_TIMEOUT: Duration = Duration::from_secs(30);
const BACKFILL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_BACKLOG: usize = 128;
const EVENT_BACKLOG: usize = 128;

/// Authenticated Commonware identity used to address one manifest peer.
pub type P2pPeerId = PublicKey;

type CommonwareSender = lookup::Sender<PublicKey, commonware_runtime::tokio::Context>;
type OutstandingBackfillResponses = Arc<Mutex<HashMap<PublicKey, Instant>>>;

struct P2pSenders {
    blocks: CommonwareSender,
    backfill_requests: CommonwareSender,
    backfill_blocks: CommonwareSender,
    backfill_completions: CommonwareSender,
}

/// Fully validated configuration for one node's Zone P2P runtime.
#[derive(Clone)]
pub struct P2pConfig {
    manifest: Arc<ZoneManifest>,
    ed25519_identity: Ed25519Identity,
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
        listen: SocketAddr,
        bypass_ip_check: bool,
        expected_zone_id: u32,
        asserted_role: Option<Role>,
    ) -> eyre::Result<Self> {
        let ed25519_identity = Ed25519Identity::read_from_file(ed25519_key_path)?;
        let manifest = ZoneManifest::read_from_file(manifest_path)?;
        validate_ip_check_configuration(&manifest, bypass_ip_check)?;
        let role = manifest.validate_node(
            expected_zone_id,
            &ed25519_identity.ed25519_public_key(),
            asserted_role,
        )?;
        Ok(Self {
            manifest: Arc::new(manifest),
            ed25519_identity,
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
    /// Ask the role-appropriate peers for canonical blocks beginning at `start`.
    RequestBackfill { start: u64 },
    /// Return one canonical block to the peer that requested it.
    SendBackfillBlock { peer: PublicKey, block: Vec<u8> },
    /// Finish one page of a backfill response and advertise the responder's snapshot tip.
    CompleteBackfill { peer: PublicKey, tip: u64 },
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
    /// An authenticated peer requested canonical blocks beginning at `start`.
    BackfillRequested { peer: PublicKey, start: u64 },
    /// A sealed canonical block was returned by an eligible backfill peer.
    BackfillBlockReceived { peer: PublicKey, block: Vec<u8> },
    /// The responder sent all blocks available in this response page.
    BackfillCompleted { peer: PublicKey, tip: u64 },
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
        let (block_sender, block_receiver) =
            commonware.register(BLOCK_CHANNEL, network::block_quota(), BLOCK_BACKLOG);
        let (backfill_request_sender, backfill_request_receiver) = commonware.register(
            BACKFILL_REQUEST_CHANNEL,
            network::block_quota(),
            BLOCK_BACKLOG,
        );
        let (backfill_block_sender, backfill_block_receiver) = commonware.register(
            BACKFILL_BLOCK_CHANNEL,
            network::block_quota(),
            BLOCK_BACKLOG,
        );
        let (backfill_complete_sender, backfill_complete_receiver) = commonware.register(
            BACKFILL_COMPLETE_CHANNEL,
            network::block_quota(),
            BLOCK_BACKLOG,
        );
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

        let followers: Vec<PublicKey> = config
            .manifest
            .nodes()
            .iter()
            .filter(|node| config.manifest.role_of(node.ed25519_public_key()) == Some(Role::Follower))
            .map(|node| node.ed25519_public_key().clone())
            .collect();
        let backfill_peers = match config.role {
            Role::Leader => followers.clone(),
            Role::Follower => vec![config.manifest.leader_ed25519_public_key().clone()],
        };
        let outstanding_backfill_responses = Arc::new(Mutex::new(HashMap::new()));
        let command_loop = run_commands(
            config.role,
            followers,
            backfill_peers,
            outstanding_backfill_responses.clone(),
            P2pSenders {
                blocks: block_sender,
                backfill_requests: backfill_request_sender,
                backfill_blocks: backfill_block_sender,
                backfill_completions: backfill_complete_sender,
            },
            command_rx,
        );
        tokio::pin!(command_loop);

        let receive_loop = run_receivers(
            config.role,
            config.manifest,
            block_receiver,
            backfill_request_receiver,
            backfill_block_receiver,
            backfill_complete_receiver,
            outstanding_backfill_responses,
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
    backfill_peers: Vec<PublicKey>,
    outstanding_backfill_responses: OutstandingBackfillResponses,
    mut senders: P2pSenders,
    mut commands: mpsc::Receiver<P2pCommand>,
) -> eyre::Result<()> {
    while let Some(command) = commands.recv().await {
        match command {
            P2pCommand::BroadcastBlock(block) => {
                if role != Role::Leader {
                    warn!(target: "zone::p2p", "Ignoring live block broadcast command on follower");
                    continue;
                }
                if block.len() > MAX_MESSAGE_SIZE as usize {
                    error!(target: "zone::p2p", block_size_bytes = block.len(), max_message_size_bytes = MAX_MESSAGE_SIZE, "Canonical block exceeds the P2P message size limit; block was not broadcast");
                    continue;
                }
                let sent = tokio::time::timeout(BROADCAST_RETRY_TIMEOUT, async {
                    loop {
                        let sent = senders.blocks
                            .send(Recipients::Some(followers.clone()), block.clone(), true)
                            .await
                            .map_err(|err| eyre::eyre!("failed broadcasting zone block: {err}"))?;
                        if !sent.is_empty() || followers.is_empty() {
                            return Ok::<_, eyre::Report>(sent);
                        }
                        debug!(target: "zone::p2p", "No followers are connected; retrying canonical block broadcast");
                        tokio::time::sleep(BROADCAST_RETRY_INTERVAL).await;
                    }
                }).await;
                let sent = match sent {
                    Ok(sent) => sent?,
                    Err(_) => {
                        warn!(target: "zone::p2p", timeout_secs = BROADCAST_RETRY_TIMEOUT.as_secs(), "No followers connected before block broadcast timed out");
                        continue;
                    }
                };
                if sent.len() != followers.len() {
                    debug!(target: "zone::p2p", connected = sent.len(), configured = followers.len(), "Some followers are not connected; block was not sent to them");
                }
            }
            P2pCommand::RequestBackfill { start } => {
                let now = Instant::now();
                let request_peers = {
                    let outstanding = outstanding_backfill_responses.lock().await;
                    backfill_peers
                        .iter()
                        .filter(|peer| {
                            outstanding.get(*peer).is_none_or(|requested_at| {
                                now.duration_since(*requested_at) >= BACKFILL_RESPONSE_TIMEOUT
                            })
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                };
                if request_peers.is_empty() {
                    debug!(target: "zone::p2p", start, configured = backfill_peers.len(), "Skipping block backfill request because all eligible peers already have outstanding responses");
                    continue;
                }
                let sent = senders
                    .backfill_requests
                    .send(
                        Recipients::Some(request_peers.clone()),
                        start.to_be_bytes().to_vec(),
                        true,
                    )
                    .await
                    .map_err(|err| eyre::eyre!("failed requesting block backfill: {err}"))?;
                {
                    let mut outstanding = outstanding_backfill_responses.lock().await;
                    let sent_at = Instant::now();
                    for peer in &sent {
                        outstanding.insert(peer.clone(), sent_at);
                    }
                }
                debug!(target: "zone::p2p", start, connected = sent.len(), requested = request_peers.len(), configured = backfill_peers.len(), "Sent block backfill request");
            }
            P2pCommand::SendBackfillBlock { peer, block } => {
                if block.len() > MAX_MESSAGE_SIZE as usize {
                    error!(target: "zone::p2p", block_size_bytes = block.len(), max_message_size_bytes = MAX_MESSAGE_SIZE, "Backfill block exceeds the P2P message size limit");
                    continue;
                }
                senders
                    .backfill_blocks
                    .send(Recipients::Some(vec![peer]), block, true)
                    .await
                    .map_err(|err| eyre::eyre!("failed sending backfill block: {err}"))?;
            }
            P2pCommand::CompleteBackfill { peer, tip } => {
                senders
                    .backfill_completions
                    .send(
                        Recipients::Some(vec![peer]),
                        tip.to_be_bytes().to_vec(),
                        true,
                    )
                    .await
                    .map_err(|err| eyre::eyre!("failed completing block backfill: {err}"))?;
            }
        }
    }

    Err(eyre::eyre!("P2P command channel closed unexpectedly"))
}

async fn run_receivers(
    role: Role,
    manifest: Arc<ZoneManifest>,
    mut block_receiver: lookup::Receiver<PublicKey>,
    mut backfill_request_receiver: lookup::Receiver<PublicKey>,
    mut backfill_block_receiver: lookup::Receiver<PublicKey>,
    mut backfill_complete_receiver: lookup::Receiver<PublicKey>,
    outstanding_backfill_responses: OutstandingBackfillResponses,
    events: mpsc::Sender<P2pEvent>,
) -> eyre::Result<()> {
    let leader = manifest.leader_ed25519_public_key().clone();
    loop {
        let event = tokio::select! {
            result = block_receiver.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("block channel receive failed: {err}"))?;
                if role == Role::Leader || peer != leader {
                    warn!(target: "zone::p2p", %peer, "Ignoring live block from non-leader");
                    continue;
                }
                P2pEvent::BlockReceived { leader_ed25519_public_key: peer, block: bytes.into() }
            }
            result = backfill_request_receiver.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("backfill request receive failed: {err}"))?;
                let Ok(bytes): Result<[u8; 8], _> = bytes.as_ref().try_into() else {
                    warn!(target: "zone::p2p", %peer, size = bytes.len(), "Ignoring malformed backfill request");
                    continue;
                };
                P2pEvent::BackfillRequested { peer, start: u64::from_be_bytes(bytes) }
            }
            result = backfill_block_receiver.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("backfill block receive failed: {err}"))?;
                let eligible = match role { Role::Leader => peer != leader, Role::Follower => peer == leader };
                if !eligible {
                    warn!(target: "zone::p2p", %peer, "Ignoring backfill block from ineligible peer");
                    continue;
                }
                if !outstanding_backfill_responses
                    .lock()
                    .await
                    .contains_key(&peer)
                {
                    warn!(target: "zone::p2p", %peer, "Ignoring unsolicited backfill block");
                    continue;
                }
                P2pEvent::BackfillBlockReceived { peer, block: bytes.into() }
            }
            result = backfill_complete_receiver.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("backfill completion receive failed: {err}"))?;
                let eligible = match role { Role::Leader => peer != leader, Role::Follower => peer == leader };
                if !eligible {
                    warn!(target: "zone::p2p", %peer, "Ignoring backfill completion from ineligible peer");
                    continue;
                }
                if !outstanding_backfill_responses
                    .lock()
                    .await
                    .contains_key(&peer)
                {
                    warn!(target: "zone::p2p", %peer, "Ignoring unsolicited backfill completion");
                    continue;
                }
                let Ok(bytes): Result<[u8; 8], _> = bytes.as_ref().try_into() else {
                    outstanding_backfill_responses.lock().await.remove(&peer);
                    warn!(target: "zone::p2p", %peer, size = bytes.len(), "Ignoring malformed backfill completion");
                    continue;
                };
                {
                    let mut outstanding = outstanding_backfill_responses.lock().await;
                    if outstanding.remove(&peer).is_none() {
                        warn!(target: "zone::p2p", %peer, "Ignoring unsolicited backfill completion");
                        continue;
                    }
                }
                P2pEvent::BackfillCompleted { peer, tip: u64::from_be_bytes(bytes) }
            }
        };
        events
            .send(event)
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
    use crate::{P2pNetworkId, ZoneManifest, identity::Ed25519Identity, network::MAX_MESSAGE_SIZE};

    fn available_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    fn ed25519_identity(seed: u64) -> Ed25519Identity {
        let key = PrivateKey::from_seed(seed);
        Ed25519Identity::from_hex(&const_hex::encode_prefixed(key.encode().as_ref())).unwrap()
    }

    async fn assert_no_backfill_response_events(
        events: &mut tokio::sync::mpsc::Receiver<P2pEvent>,
        duration: Duration,
        context: &str,
    ) {
        let result = tokio::time::timeout(duration, async {
            loop {
                match events.recv().await {
                    Some(P2pEvent::BackfillBlockReceived { .. }) => {
                        panic!("{context}: accepted unsolicited backfill block")
                    }
                    Some(P2pEvent::BackfillCompleted { .. }) => {
                        panic!("{context}: accepted unsolicited backfill completion")
                    }
                    Some(_) => {}
                    None => return,
                }
            }
        })
        .await;
        assert!(
            result.is_err(),
            "{context}: event channel closed while checking for unsolicited backfill response"
        );
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
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\naddress = \"node-{index}.zone.local:9200\"\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref())
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
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\naddress = \"{address}\"\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref())
            ));
        }
        let manifest = Arc::new(ZoneManifest::parse(&input).unwrap());
        let leader_peer = identities[0].ed25519_public_key();
        let first_follower_peer = identities[1].ed25519_public_key();
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

        let leader_commands = handles[0].parts.as_ref().unwrap().commands.clone();
        let follower_commands = handles[1].parts.as_ref().unwrap().commands.clone();
        leader_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: first_follower_peer.clone(),
                block: block.clone(),
            })
            .await
            .unwrap();
        leader_commands
            .send(P2pCommand::CompleteBackfill {
                peer: first_follower_peer.clone(),
                tip: 99,
            })
            .await
            .unwrap();
        assert_no_backfill_response_events(
            handles[1].events_mut(),
            Duration::from_secs(2),
            "follower without an outstanding backfill request",
        )
        .await;

        follower_commands
            .send(P2pCommand::RequestBackfill { start: 7 })
            .await
            .unwrap();
        let requesting_peer = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::BackfillRequested { peer, start: 7 }) =
                    handles[0].events_mut().recv().await
                {
                    return peer;
                }
            }
        })
        .await
        .expect("leader did not receive backfill request");

        follower_commands
            .send(P2pCommand::RequestBackfill { start: 8 })
            .await
            .unwrap();
        let duplicate_request = tokio::time::timeout(Duration::from_millis(750), async {
            loop {
                if let Some(P2pEvent::BackfillRequested { start: 8, .. }) =
                    handles[0].events_mut().recv().await
                {
                    return;
                }
            }
        })
        .await;
        assert!(
            duplicate_request.is_err(),
            "follower sent a duplicate backfill request while the first response was outstanding"
        );

        follower_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: leader_peer.clone(),
                block: block.clone(),
            })
            .await
            .unwrap();
        follower_commands
            .send(P2pCommand::CompleteBackfill {
                peer: leader_peer.clone(),
                tip: 100,
            })
            .await
            .unwrap();
        assert_no_backfill_response_events(
            handles[0].events_mut(),
            Duration::from_secs(2),
            "leader without an outstanding backfill request",
        )
        .await;

        leader_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: requesting_peer.clone(),
                block: block.clone(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::BackfillBlockReceived {
                    block: received, ..
                }) = handles[1].events_mut().recv().await
                {
                    assert_eq!(received, block);
                    return;
                }
            }
        })
        .await
        .expect("follower did not receive requested backfill block");
        leader_commands
            .send(P2pCommand::CompleteBackfill {
                peer: requesting_peer,
                tip: 9,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match handles[1].events_mut().recv().await {
                    Some(P2pEvent::BackfillCompleted { tip: 9, .. }) => {
                        return;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("follower did not receive requested backfill completion");

        leader_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: first_follower_peer.clone(),
                block: block.clone(),
            })
            .await
            .unwrap();
        leader_commands
            .send(P2pCommand::CompleteBackfill {
                peer: first_follower_peer.clone(),
                tip: 10,
            })
            .await
            .unwrap();
        assert_no_backfill_response_events(
            handles[1].events_mut(),
            Duration::from_secs(2),
            "follower after its backfill request completed",
        )
        .await;

        leader_commands
            .send(P2pCommand::RequestBackfill { start: 11 })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::BackfillRequested { start: 11, .. }) =
                    handles[1].events_mut().recv().await
                {
                    return;
                }
            }
        })
        .await
        .expect("follower did not receive recovering leader's backfill request");

        follower_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: leader_peer.clone(),
                block: block.clone(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::BackfillBlockReceived {
                    peer,
                    block: received,
                }) = handles[0].events_mut().recv().await
                {
                    assert!(peer == first_follower_peer);
                    assert_eq!(received, block);
                    return;
                }
            }
        })
        .await
        .expect("leader did not receive requested backfill block");
        follower_commands
            .send(P2pCommand::CompleteBackfill {
                peer: leader_peer,
                tip: 12,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::BackfillCompleted { peer, tip: 12 }) =
                    handles[0].events_mut().recv().await
                {
                    assert!(peer == first_follower_peer);
                    return;
                }
            }
        })
        .await
        .expect("leader did not receive requested backfill completion");

        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
                .await
                .expect("P2P runtime did not stop")
                .expect("P2P runtime failed");
        }
    }
}
