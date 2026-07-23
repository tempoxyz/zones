use std::{
    collections::HashMap,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::Address as EthereumAddress;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{
    AddressableManager as _, Receiver as _, Recipients, Sender as _, authenticated::lookup,
};
use commonware_runtime::{Runner as _, Spawner as _};
use eyre::WrapErr as _;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    P2pNetworkId, Role, ZoneManifest,
    identity::{Ed25519Identity, Secp256k1Identity},
    network::{
        self, BACKFILL_REQUEST_CHANNEL, BACKFILL_RESPONSE_CHANNEL, BLOCK_BACKLOG, BLOCK_CHANNEL,
        MAX_MESSAGE_SIZE, SETTLEMENT_PROPOSAL_CHANNEL, SETTLEMENT_SIGNATURE_CHANNEL,
    },
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const BROADCAST_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const BROADCAST_RETRY_TIMEOUT: Duration = Duration::from_secs(30);
const BACKFILL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const COMMAND_BACKLOG: usize = 128;
const EVENT_BACKLOG: usize = 128;
const BACKFILL_BLOCK_FRAME: u8 = 0;
const BACKFILL_COMPLETE_FRAME: u8 = 1;

/// Authenticated Commonware identity used to address one manifest peer.
pub type P2pPeerId = PublicKey;

type CommonwareSender = lookup::Sender<PublicKey, commonware_runtime::tokio::Context>;
type CommonwareReceiver = lookup::Receiver<PublicKey>;
type SharedBackfillLifecycle = Arc<Mutex<BackfillJob>>;

#[derive(Debug, Clone, Copy)]
struct OutstandingBackfill {
    request_id: u64,
    sent_at: Instant,
}

#[derive(Debug, Default)]
struct BackfillJob {
    next_request_id: u64,
    outstanding: HashMap<PublicKey, OutstandingBackfill>,
}

impl BackfillJob {
    fn begin_request(
        &mut self,
        peers: &[PublicKey],
        now: Instant,
    ) -> Option<(u64, Vec<PublicKey>)> {
        let request_peers = peers
            .iter()
            .filter(|peer| {
                self.outstanding.get(*peer).is_none_or(|request| {
                    now.duration_since(request.sent_at) >= BACKFILL_RESPONSE_TIMEOUT
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if request_peers.is_empty() {
            return None;
        }

        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        for peer in &request_peers {
            self.outstanding.insert(
                peer.clone(),
                OutstandingBackfill {
                    request_id,
                    sent_at: now,
                },
            );
        }
        Some((request_id, request_peers))
    }

    fn finish_send(&mut self, request_id: u64, sent: &[PublicKey]) {
        self.outstanding
            .retain(|peer, request| request.request_id != request_id || sent.contains(peer));
    }

    fn cancel_request(&mut self, request_id: u64) {
        self.outstanding
            .retain(|_, request| request.request_id != request_id);
    }

    fn accepts(&self, peer: &PublicKey, request_id: u64, now: Instant) -> bool {
        self.outstanding.get(peer).is_some_and(|request| {
            request.request_id == request_id
                && now.duration_since(request.sent_at) < BACKFILL_RESPONSE_TIMEOUT
        })
    }

    fn complete(&mut self, peer: &PublicKey, request_id: u64, now: Instant) -> bool {
        if !self.accepts(peer, request_id, now) {
            return false;
        }
        self.outstanding.remove(peer);
        true
    }
}

struct P2pSenders {
    blocks: CommonwareSender,
    settlement_proposals: CommonwareSender,
    settlement_signatures: CommonwareSender,
    backfill_requests: CommonwareSender,
    backfill_responses: CommonwareSender,
}

struct P2pReceivers {
    blocks: CommonwareReceiver,
    settlement_proposals: CommonwareReceiver,
    settlement_signatures: CommonwareReceiver,
    backfill_requests: CommonwareReceiver,
    backfill_responses: CommonwareReceiver,
}

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

    /// Signer used for EIP-712 zone-block attestations.
    pub fn block_attestation_signer(&self) -> alloy_signer_local::PrivateKeySigner {
        self.secp256k1_identity.signer()
    }

    /// Expected attestation address for every peer.
    pub fn block_attestation_addresses(&self) -> HashMap<PublicKey, EthereumAddress> {
        self.manifest
            .nodes()
            .iter()
            .map(|node| (node.ed25519_public_key().clone(), node.secp256k1_address()))
            .collect()
    }

    /// Local socket bound by Commonware.
    pub const fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Zone ID included in each block attestation.
    pub fn zone_id(&self) -> u32 {
        self.manifest.zone_id()
    }

    /// Registered signer-set version included in each block attestation.
    pub fn sequencer_set_version(&self) -> u64 {
        self.manifest.sequencer_set_version()
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
    /// Broadcast one ABI-encoded settlement proposal to all followers.
    BroadcastSettlementProposal(Vec<u8>),
    /// Return one ABI-encoded settlement signature to the leader.
    SendSettlementSignature(Vec<u8>),
    /// Ask the role-appropriate peers for canonical blocks beginning at `start`.
    RequestBackfill { start: u64 },
    /// Return one canonical block to the peer that requested it.
    SendBackfillBlock {
        peer: PublicKey,
        request_id: u64,
        block: Vec<u8>,
    },
    /// Finish one page of a backfill response and advertise the responder's snapshot tip.
    CompleteBackfill {
        peer: PublicKey,
        request_id: u64,
        tip: u64,
    },
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
    /// A follower received a proposed settlement statement from the leader.
    SettlementProposalReceived {
        leader: PublicKey,
        proposal: Vec<u8>,
    },
    /// The leader received a settlement signature from a follower.
    SettlementSignatureReceived {
        follower: PublicKey,
        signature: Vec<u8>,
    },
    /// An authenticated peer requested canonical blocks beginning at `start`.
    BackfillRequested {
        peer: PublicKey,
        request_id: u64,
        start: u64,
    },
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
        let (settlement_proposal_sender, settlement_proposal_receiver) = commonware.register(
            SETTLEMENT_PROPOSAL_CHANNEL,
            network::settlement_quota(),
            BLOCK_BACKLOG,
        );
        let (settlement_signature_sender, settlement_signature_receiver) = commonware.register(
            SETTLEMENT_SIGNATURE_CHANNEL,
            network::settlement_quota(),
            BLOCK_BACKLOG,
        );

        // The backfill request and responses are on separate channels
        let (backfill_request_sender, backfill_request_receiver) = commonware.register(
            BACKFILL_REQUEST_CHANNEL,
            network::backfill_request_quota(),
            BLOCK_BACKLOG,
        );
        let (backfill_response_sender, backfill_response_receiver) = commonware.register(
            BACKFILL_RESPONSE_CHANNEL,
            network::backfill_response_quota(),
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
        let leader = config.manifest.leader_ed25519_public_key().clone();

        let backfill_peers = match config.role {
            // A recovering leader can backfill from all followers
            Role::Leader => followers.clone(),

            // A recovering follower can backfill from the canonical leader
            Role::Follower => vec![config.manifest.leader_ed25519_public_key().clone()],
        };

        let backfill_lifecycle = Arc::new(Mutex::new(BackfillJob::default()));
        let command_loop = run_commands(
            config.role,
            leader,
            followers,
            backfill_peers,
            backfill_lifecycle.clone(),
            P2pSenders {
                blocks: block_sender,
                settlement_proposals: settlement_proposal_sender,
                settlement_signatures: settlement_signature_sender,
                backfill_requests: backfill_request_sender,
                backfill_responses: backfill_response_sender,
            },
            command_rx,
        );
        tokio::pin!(command_loop);

        let receive_loop = run_receivers(
            config.role,
            config.manifest,
            P2pReceivers {
                blocks: block_receiver,
                settlement_proposals: settlement_proposal_receiver,
                settlement_signatures: settlement_signature_receiver,
                backfill_requests: backfill_request_receiver,
                backfill_responses: backfill_response_receiver,
            },
            backfill_lifecycle,
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
    leader: PublicKey,
    followers: Vec<PublicKey>,
    backfill_peers: Vec<PublicKey>,
    backfill_job: SharedBackfillLifecycle,
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

            P2pCommand::BroadcastSettlementProposal(proposal) => {
                if role != Role::Leader {
                    warn!(target: "zone::p2p", "Ignoring settlement proposal command on follower");
                    continue;
                }
                senders
                    .settlement_proposals
                    .send(Recipients::Some(followers.clone()), proposal, true)
                    .await
                    .wrap_err("failed broadcasting settlement proposal")?;
            }

            P2pCommand::SendSettlementSignature(signature) => {
                if role != Role::Follower {
                    warn!(target: "zone::p2p", "Ignoring settlement signature command on leader");
                    continue;
                }
                senders
                    .settlement_signatures
                    .send(Recipients::Some(vec![leader.clone()]), signature, true)
                    .await
                    .wrap_err("failed sending settlement signature")?;
            }

            P2pCommand::RequestBackfill { start } => {
                let now = Instant::now();
                let request = {
                    backfill_job
                        .lock()
                        .await
                        .begin_request(&backfill_peers, now)
                };
                let Some((request_id, request_peers)) = request else {
                    debug!(target: "zone::p2p", start, configured = backfill_peers.len(), "Skipping block backfill request because all eligible peers already have outstanding responses");
                    continue;
                };
                let mut request_frame = Vec::with_capacity(16);
                request_frame.extend_from_slice(&request_id.to_be_bytes());
                request_frame.extend_from_slice(&start.to_be_bytes());
                let sent = match senders
                    .backfill_requests
                    .send(Recipients::Some(request_peers.clone()), request_frame, true)
                    .await
                {
                    Ok(sent) => sent,
                    Err(err) => {
                        backfill_job.lock().await.cancel_request(request_id);
                        return Err(eyre::eyre!("failed requesting block backfill: {err}"));
                    }
                };
                backfill_job.lock().await.finish_send(request_id, &sent);
                debug!(target: "zone::p2p", request_id, start, connected = sent.len(), requested = request_peers.len(), configured = backfill_peers.len(), "Sent block backfill request");
            }

            P2pCommand::SendBackfillBlock {
                peer,
                request_id,
                block,
            } => {
                if block.len().saturating_add(9) > MAX_MESSAGE_SIZE as usize {
                    error!(target: "zone::p2p", block_size_bytes = block.len(), max_frame_size_bytes = MAX_MESSAGE_SIZE, "Backfill block exceeds the P2P response frame size limit");
                    continue;
                }
                let mut frame = Vec::with_capacity(block.len() + 9);
                frame.push(BACKFILL_BLOCK_FRAME);
                frame.extend_from_slice(&request_id.to_be_bytes());
                frame.extend_from_slice(&block);
                senders
                    .backfill_responses
                    .send(Recipients::Some(vec![peer]), frame, true)
                    .await
                    .map_err(|err| eyre::eyre!("failed sending backfill block: {err}"))?;
            }

            P2pCommand::CompleteBackfill {
                peer,
                request_id,
                tip,
            } => {
                let mut frame = Vec::with_capacity(17);
                frame.push(BACKFILL_COMPLETE_FRAME);
                frame.extend_from_slice(&request_id.to_be_bytes());
                frame.extend_from_slice(&tip.to_be_bytes());
                senders
                    .backfill_responses
                    .send(Recipients::Some(vec![peer]), frame, true)
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
    receivers: P2pReceivers,
    backfill_job: SharedBackfillLifecycle,
    events: mpsc::Sender<P2pEvent>,
) -> eyre::Result<()> {
    let P2pReceivers {
        mut blocks,
        mut settlement_proposals,
        mut settlement_signatures,
        mut backfill_requests,
        mut backfill_responses,
    } = receivers;

    let leader = manifest.leader_ed25519_public_key().clone();
    loop {
        let event = tokio::select! {
            // Got a block
            result = blocks.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("block channel receive failed: {err}"))?;
                if role == Role::Leader || peer != leader {
                    warn!(target: "zone::p2p", %peer, "Ignoring live block from non-leader");
                    continue;
                }
                P2pEvent::BlockReceived { leader_ed25519_public_key: peer, block: bytes.into() }
            }

            // Got a settlement proposal at a batch boundary
            result = settlement_proposals.recv() => {
                let (peer, bytes) = result.wrap_err("settlement proposal channel receive failed")?;
                if role != Role::Follower || peer != leader {
                    warn!(target: "zone::p2p", %peer, "Ignoring settlement proposal from ineligible peer");
                    continue;
                }
                P2pEvent::SettlementProposalReceived { leader: peer, proposal: bytes.into() }
            }

            // Got a response from a follower to the settlement proposal
            result = settlement_signatures.recv() => {
                let (peer, bytes) = result.wrap_err("settlement signature channel receive failed")?;
                if role != Role::Leader || peer == leader {
                    warn!(target: "zone::p2p", %peer, "Ignoring settlement signature from ineligible peer");
                    continue;
                }
                P2pEvent::SettlementSignatureReceived { follower: peer, signature: bytes.into() }
            }

            // Got backfill request
            result = backfill_requests.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("backfill request receive failed: {err}"))?;
                let Ok(bytes): Result<[u8; 16], _> = bytes.as_ref().try_into() else {
                    warn!(target: "zone::p2p", %peer, size = bytes.len(), "Ignoring malformed backfill request");
                    continue;
                };
                let request_id = u64::from_be_bytes(bytes[..8].try_into().expect("fixed-size request ID"));
                let start = u64::from_be_bytes(bytes[8..].try_into().expect("fixed-size backfill start"));
                P2pEvent::BackfillRequested { peer, request_id, start }
            }

            // Got backfill response (for an existing request)
            result = backfill_responses.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("backfill response receive failed: {err}"))?;
                let eligible = match role { Role::Leader => peer != leader, Role::Follower => peer == leader };
                if !eligible {
                    warn!(target: "zone::p2p", %peer, "Ignoring backfill response from ineligible peer");
                    continue;
                }
                let Some((&frame_kind, frame_payload)) = bytes.as_ref().split_first() else {
                    warn!(target: "zone::p2p", %peer, "Ignoring empty backfill response frame");
                    continue;
                };
                let Some((request_id_bytes, payload)) = frame_payload.split_at_checked(8) else {
                    warn!(target: "zone::p2p", %peer, size = frame_payload.len(), "Ignoring backfill response without a request ID");
                    continue;
                };
                let request_id = u64::from_be_bytes(request_id_bytes.try_into().expect("fixed-size request ID"));
                let received_at = Instant::now();

                let mut backfill_job = backfill_job.lock().await;
                match frame_kind {
                    BACKFILL_BLOCK_FRAME => {
                        if !backfill_job
                            .accepts(&peer, request_id, received_at)
                        {
                            warn!(target: "zone::p2p", %peer, request_id, "Ignoring unsolicited or stale backfill block");
                            continue;
                        }
                        P2pEvent::BackfillBlockReceived { peer, block: payload.to_vec() }
                    }
                    BACKFILL_COMPLETE_FRAME => {
                        let accepted = backfill_job.complete(&peer, request_id, received_at);
                        if !accepted {
                            warn!(target: "zone::p2p", %peer, request_id, "Ignoring unsolicited or stale backfill completion");
                            continue;
                        }
                        let Ok(tip_bytes): Result<[u8; 8], _> = payload.try_into() else {
                            warn!(target: "zone::p2p", %peer, request_id, size = payload.len(), "Ignoring malformed backfill completion");
                            continue;
                        };
                        P2pEvent::BackfillCompleted { peer, tip: u64::from_be_bytes(tip_bytes) }
                    }
                    _ => {
                        warn!(target: "zone::p2p", %peer, frame_kind, "Ignoring backfill response with unknown frame kind");
                        continue;
                    }
                }
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
        time::{Duration, Instant},
    };

    use alloy_primitives::address;
    use commonware_codec::Encode as _;
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::{
        BACKFILL_RESPONSE_TIMEOUT, BackfillJob, P2pCommand, P2pConfig, P2pEvent, spawn_p2p,
        validate_ip_check_configuration,
    };
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
    fn stale_response_cannot_complete_replacement_request() {
        let peer = ed25519_identity(1).ed25519_public_key();
        let now = Instant::now();
        let mut lifecycle = BackfillJob::default();

        let (first_id, peers) = lifecycle
            .begin_request(std::slice::from_ref(&peer), now)
            .unwrap();
        lifecycle.finish_send(first_id, &peers);
        assert!(lifecycle.accepts(&peer, first_id, now));
        let halfway = now + BACKFILL_RESPONSE_TIMEOUT / 2;
        assert!(lifecycle.accepts(&peer, first_id, halfway));
        assert!(
            lifecycle
                .begin_request(std::slice::from_ref(&peer), halfway)
                .is_none()
        );

        let expired_at = now + BACKFILL_RESPONSE_TIMEOUT;
        assert!(!lifecycle.accepts(&peer, first_id, expired_at));
        assert!(!lifecycle.complete(&peer, first_id, expired_at));

        let (replacement_id, peers) = lifecycle
            .begin_request(std::slice::from_ref(&peer), expired_at)
            .unwrap();
        lifecycle.finish_send(replacement_id, &peers);
        assert_ne!(first_id, replacement_id);
        assert!(!lifecycle.accepts(&peer, first_id, expired_at));
        assert!(!lifecycle.complete(&peer, first_id, expired_at));
        assert!(lifecycle.accepts(&peer, replacement_id, expired_at));
        assert!(lifecycle.complete(&peer, replacement_id, expired_at));
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
        let leader_peer = identities[0].ed25519_public_key();
        let first_follower_peer = identities[1].ed25519_public_key();
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

        let leader_commands = handles[0].parts.as_ref().unwrap().commands.clone();
        let follower_commands = handles[1].parts.as_ref().unwrap().commands.clone();
        let proposal = vec![0x10, 0x20];
        leader_commands
            .send(P2pCommand::BroadcastSettlementProposal(proposal.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::SettlementProposalReceived {
                    leader,
                    proposal: received,
                }) = handles[1].events_mut().recv().await
                {
                    assert_eq!(leader, leader_peer);
                    assert_eq!(received, proposal);
                    return;
                }
            }
        })
        .await
        .expect("follower did not receive settlement proposal");

        let settlement_signature = vec![0x30, 0x40];
        follower_commands
            .send(P2pCommand::SendSettlementSignature(
                settlement_signature.clone(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::SettlementSignatureReceived {
                    follower,
                    signature,
                }) = handles[0].events_mut().recv().await
                {
                    assert_eq!(follower, first_follower_peer);
                    assert_eq!(signature, settlement_signature);
                    return;
                }
            }
        })
        .await
        .expect("leader did not receive settlement signature");

        leader_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: first_follower_peer.clone(),
                request_id: 0,
                block: block.clone(),
            })
            .await
            .unwrap();
        leader_commands
            .send(P2pCommand::CompleteBackfill {
                peer: first_follower_peer.clone(),
                request_id: 0,
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
        let (requesting_peer, follower_request_id) =
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(P2pEvent::BackfillRequested {
                        peer,
                        request_id,
                        start: 7,
                    }) = handles[0].events_mut().recv().await
                    {
                        return (peer, request_id);
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
                request_id: 0,
                block: block.clone(),
            })
            .await
            .unwrap();
        follower_commands
            .send(P2pCommand::CompleteBackfill {
                peer: leader_peer.clone(),
                request_id: 0,
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
                request_id: follower_request_id,
                block: block.clone(),
            })
            .await
            .unwrap();
        let second_block = vec![0xf8, 0x02, 0x80];
        leader_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: requesting_peer.clone(),
                request_id: follower_request_id,
                block: second_block.clone(),
            })
            .await
            .unwrap();
        leader_commands
            .send(P2pCommand::CompleteBackfill {
                peer: requesting_peer,
                request_id: follower_request_id,
                tip: 9,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            let expected_blocks = [block.clone(), second_block];
            let mut received_blocks = 0;
            loop {
                match handles[1].events_mut().recv().await {
                    Some(P2pEvent::BackfillBlockReceived { block, .. }) => {
                        assert_eq!(block, expected_blocks[received_blocks]);
                        received_blocks += 1;
                    }
                    Some(P2pEvent::BackfillCompleted { tip: 9, .. }) => {
                        assert_eq!(received_blocks, expected_blocks.len());
                        return;
                    }
                    Some(_) => {}
                    None => panic!("follower event channel closed"),
                }
            }
        })
        .await
        .expect("follower did not receive ordered backfill response");

        leader_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: first_follower_peer.clone(),
                request_id: follower_request_id,
                block: block.clone(),
            })
            .await
            .unwrap();
        leader_commands
            .send(P2pCommand::CompleteBackfill {
                peer: first_follower_peer.clone(),
                request_id: follower_request_id,
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
        let leader_request_id = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::BackfillRequested {
                    request_id,
                    start: 11,
                    ..
                }) = handles[1].events_mut().recv().await
                {
                    return request_id;
                }
            }
        })
        .await
        .expect("follower did not receive recovering leader's backfill request");

        follower_commands
            .send(P2pCommand::SendBackfillBlock {
                peer: leader_peer.clone(),
                request_id: leader_request_id,
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
                request_id: leader_request_id,
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
