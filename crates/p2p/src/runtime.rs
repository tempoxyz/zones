use std::{
    collections::HashMap,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address as EthereumAddress, B256};
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
    LeadershipSchedule, P2pNetworkId, Role, ZoneManifest,
    identity::{Ed25519Identity, Secp256k1Identity},
    network::{
        self, BACKFILL_REQUEST_CHANNEL, BACKFILL_RESPONSE_CHANNEL, BLOCK_BACKLOG, BLOCK_CHANNEL,
        MAX_MESSAGE_SIZE, SETTLEMENT_PROPOSAL_CHANNEL, SETTLEMENT_SIGNATURE_CHANNEL,
        TRANSACTION_BACKLOG, TRANSACTION_CHANNEL,
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

/// A peer's advertised canonical tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerTip {
    pub zone_height: u64,
    pub zone_hash: B256,
    pub tempo_block_number: u64,
    pub tempo_block_hash: B256,
}

impl PeerTip {
    const ENCODED_LEN: usize = 8 + 32 + 8 + 32;

    fn encode_into(&self, frame: &mut Vec<u8>) {
        frame.extend_from_slice(&self.zone_height.to_be_bytes());
        frame.extend_from_slice(self.zone_hash.as_slice());
        frame.extend_from_slice(&self.tempo_block_number.to_be_bytes());
        frame.extend_from_slice(self.tempo_block_hash.as_slice());
    }

    fn decode(payload: &[u8]) -> Option<Self> {
        let payload: &[u8; Self::ENCODED_LEN] = payload.try_into().ok()?;
        Some(Self {
            zone_height: u64::from_be_bytes(payload[..8].try_into().expect("fixed size")),
            zone_hash: B256::from_slice(&payload[8..40]),
            tempo_block_number: u64::from_be_bytes(payload[40..48].try_into().expect("fixed size")),
            tempo_block_hash: B256::from_slice(&payload[48..80]),
        })
    }
}

type CommonwareSender = lookup::Sender<PublicKey, commonware_runtime::tokio::Context>;
type CommonwareReceiver = lookup::Receiver<PublicKey>;
type SharedBackfillLifecycle = Arc<Mutex<BackfillJob>>;

#[derive(Debug, Clone, Copy)]
struct OutstandingBackfill {
    request_id: u64,
    sent_at: Instant,
}

impl OutstandingBackfill {
    /// Whether the response window has closed, freeing the peer to be asked again.
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.sent_at) >= BACKFILL_RESPONSE_TIMEOUT
    }
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
                self.outstanding
                    .get(*peer)
                    .is_none_or(|request| request.expired(now))
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
        self.outstanding
            .get(peer)
            .is_some_and(|request| request.request_id == request_id && !request.expired(now))
    }

    /// Whether `peer` has left a request unanswered past the response timeout.
    ///
    /// Separates "still serving" from "stopped answering", so a page in flight does not widen
    /// the source set.
    fn is_unresponsive(&self, peer: &PublicKey, now: Instant) -> bool {
        self.outstanding
            .get(peer)
            .is_some_and(|request| request.expired(now))
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
    transactions: CommonwareSender,
}

struct P2pReceivers {
    blocks: CommonwareReceiver,
    settlement_proposals: CommonwareReceiver,
    settlement_signatures: CommonwareReceiver,
    backfill_requests: CommonwareReceiver,
    backfill_responses: CommonwareReceiver,
    transactions: CommonwareReceiver,
}

/// Fully validated configuration for one node's Zone P2P runtime.
#[derive(Clone)]
pub struct P2pConfig {
    manifest: Arc<ZoneManifest>,
    ed25519_identity: Ed25519Identity,
    // This individual node key will be used to sign zone blocks for the on-chain quorum.
    // `None` on an `rpc_only` node: it never signs, so it is not provisioned with quorum key
    // material at all.
    secp256k1_identity: Option<Secp256k1Identity>,
    listen: SocketAddr,
    bypass_ip_check: bool,
    leadership: LeadershipSchedule,
}

impl P2pConfig {
    /// Loads the Commonware Ed25519 key and manifest, then validates this node's
    /// membership, zone ID, and optional role assertion.
    ///
    /// `secp256k1_key_path` is required for a quorum member and rejected for an `rpc_only`
    /// node; [`ZoneManifest::validate_node`] enforces the correspondence.
    pub fn load(
        manifest_path: impl AsRef<Path>,
        ed25519_key_path: impl AsRef<Path>,
        secp256k1_key_path: Option<impl AsRef<Path>>,
        listen: SocketAddr,
        bypass_ip_check: bool,
        expected_zone_id: u32,
        asserted_role: Option<Role>,
    ) -> eyre::Result<Self> {
        let ed25519_identity = Ed25519Identity::read_from_file(ed25519_key_path)?;
        let secp256k1_identity = secp256k1_key_path
            .map(Secp256k1Identity::read_from_file)
            .transpose()?;
        let manifest = ZoneManifest::read_from_file(manifest_path)?;
        validate_ip_check_configuration(&manifest, bypass_ip_check)?;
        manifest.validate_node(
            expected_zone_id,
            &ed25519_identity.ed25519_public_key(),
            secp256k1_identity.as_ref().map(Secp256k1Identity::address),
            asserted_role,
        )?;
        // The schedule starts uninitialized but already carries the manifest's static quorum
        // membership. The node seeds the transitions from the finalized portal snapshot at the
        // local Tempo checkpoint before any role-dependent task starts.
        let leadership = manifest.leadership_schedule();
        Ok(Self {
            manifest: Arc::new(manifest),
            ed25519_identity,
            secp256k1_identity,
            listen,
            bypass_ip_check,
            leadership,
        })
    }

    /// The shared leadership schedule for this node.
    pub fn leadership(&self) -> LeadershipSchedule {
        self.leadership.clone()
    }

    /// The validated static topology manifest.
    pub fn manifest(&self) -> &Arc<ZoneManifest> {
        &self.manifest
    }

    /// This node's Ed25519 public key used by Commonware.
    pub fn ed25519_public_key(&self) -> PublicKey {
        self.ed25519_identity.ed25519_public_key()
    }

    /// Whether this node replicates without joining the on-chain settlement quorum.
    pub fn is_rpc_only(&self) -> bool {
        self.leadership.is_rpc_only(&self.ed25519_public_key())
    }

    /// This node's address derived from its individual secp256k1 key, when it holds one.
    pub fn secp256k1_address(&self) -> Option<EthereumAddress> {
        self.secp256k1_identity
            .as_ref()
            .map(Secp256k1Identity::address)
    }

    /// Signer used for EIP-712 zone-block attestations, when this node is a quorum member.
    pub fn block_attestation_signer(&self) -> Option<alloy_signer_local::PrivateKeySigner> {
        self.secp256k1_identity
            .as_ref()
            .map(Secp256k1Identity::signer)
    }

    /// Expected attestation address for every quorum peer.
    ///
    /// RPC-only members are deliberately absent: they never sign, so a signature claiming to
    /// come from one has no registered address to match and is rejected.
    pub fn block_attestation_addresses(&self) -> HashMap<PublicKey, EthereumAddress> {
        self.manifest
            .quorum_nodes()
            .map(|(node, address)| (node.ed25519_public_key().clone(), address))
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
            .field("leadership", &self.leadership.latest())
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
    /// Broadcast one RLP-encoded sealed zone block to all nodes.
    BroadcastBlock(Vec<u8>),
    /// Broadcast one ABI-encoded settlement proposal to all followers.
    BroadcastSettlementProposal(Vec<u8>),
    /// Return one ABI-encoded settlement signature to the leader that proposed it.
    SendSettlementSignature {
        /// The peer whose proposal this signature answers.
        leader: PublicKey,
        signature: Vec<u8>,
    },
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
        tip: PeerTip,
    },
    /// Forward one canonical EIP-2718 transaction to every other quorum member.
    ForwardTransaction {
        transaction_hash: B256,
        transaction: Vec<u8>,
    },
}

/// Observable lifecycle and block events emitted by the P2P runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum P2pEvent {
    /// The network and block channel were started.
    Started {
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
    BackfillCompleted { peer: PublicKey, tip: PeerTip },
    /// A quorum member received a raw transaction from an authenticated follower.
    TransactionReceived {
        follower_ed25519_public_key: PublicKey,
        transaction: Vec<u8>,
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
        .name("zone-p2p".to_owned())
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
        let leadership = config.leadership();
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
        let (transaction_sender, transaction_receiver) = commonware.register(
            TRANSACTION_CHANNEL,
            network::transaction_quota(),
            TRANSACTION_BACKLOG,
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
            ed25519_public_key = %local_ed25519_public_key,
            listen = %config.listen,
            peers = config.manifest.nodes().len(),
            "Started P2P networking"
        );

        let _ = events
            .send(P2pEvent::Started {
                ed25519_public_key: local_ed25519_public_key.clone(),
                listen: config.listen,
            })
            .await;

        let peers: Vec<PublicKey> = config
            .manifest
            .nodes()
            .iter()
            .map(|node| node.ed25519_public_key().clone())
            .collect();

        let backfill_lifecycle = Arc::new(Mutex::new(BackfillJob::default()));
        let command_loop = run_commands(
            local_ed25519_public_key.clone(),
            peers,
            leadership.clone(),
            backfill_lifecycle.clone(),
            P2pSenders {
                blocks: block_sender,
                settlement_proposals: settlement_proposal_sender,
                settlement_signatures: settlement_signature_sender,
                backfill_requests: backfill_request_sender,
                backfill_responses: backfill_response_sender,
                transactions: transaction_sender,
            },
            command_rx,
        );
        tokio::pin!(command_loop);

        let receive_loop = run_receivers(
            local_ed25519_public_key,
            leadership,
            config.manifest,
            P2pReceivers {
                blocks: block_receiver,
                settlement_proposals: settlement_proposal_receiver,
                settlement_signatures: settlement_signature_receiver,
                backfill_requests: backfill_request_receiver,
                backfill_responses: backfill_response_receiver,
                transactions: transaction_receiver,
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
    local_ed25519_public_key: PublicKey,
    peers: Vec<PublicKey>,
    leadership: LeadershipSchedule,
    backfill_job: SharedBackfillLifecycle,
    mut senders: P2pSenders,
    mut commands: mpsc::Receiver<P2pCommand>,
) -> eyre::Result<()> {
    let others: Vec<PublicKey> = peers
        .iter()
        .filter(|peer| *peer != &local_ed25519_public_key)
        .cloned()
        .collect();

    while let Some(command) = commands.recv().await {
        if let P2pCommand::RequestBackfill { start } = command {
            // Chain data always comes from a quorum member: standbys hold the same chain but are
            // the internet-facing members, and keeping them out of every node's catch-up source
            // set is the point of the role.
            let candidates = leadership.quorum_peers(&others);
            let now = Instant::now();
            // Ask the leader alone first: backfilled blocks carry no producer claim, so a page
            // from any other member could be a valid alternative chain rather than the leader's.
            //
            // Two different failures have to widen the source set. A leader that received the
            // request and went quiet is caught by `is_unresponsive` on a later tick. A leader
            // that is not connected never receives one — the send reaches nobody and
            // `finish_send` drops its outstanding entry, so `is_unresponsive` would stay false
            // forever and every retry would re-pick it. Attempting the wider set in the same
            // pass is what keeps a leader outage from wedging catch-up entirely.
            let leader = leadership.next_anchor_record().map(|record| record.leader);
            let leader_first = match &leader {
                Some(leader) => {
                    candidates.contains(leader)
                        && !backfill_job.lock().await.is_unresponsive(leader, now)
                }
                None => false,
            };
            let mut attempts = Vec::with_capacity(2);
            if let Some(leader) = leader.filter(|_| leader_first) {
                attempts.push(vec![leader]);
            }
            attempts.push(candidates);

            for (attempt, sources) in attempts.into_iter().enumerate() {
                let leader_only = leader_first && attempt == 0;
                let request = backfill_job.lock().await.begin_request(&sources, now);
                let Some((request_id, request_peers)) = request else {
                    debug!(target: "zone::p2p", start, sources = sources.len(), leader_only, "Skipping block backfill request because all eligible peers already have outstanding responses");
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
                if sent.is_empty() {
                    debug!(target: "zone::p2p", request_id, start, requested = request_peers.len(), leader_only, "Block backfill request reached no peer");
                    continue;
                }
                if !leader_only {
                    metrics::counter!("zone_p2p_backfill_requests_without_leader_total")
                        .increment(1);
                }
                debug!(target: "zone::p2p", request_id, start, connected = sent.len(), requested = request_peers.len(), sources = sources.len(), leader_only, "Sent block backfill request");
                break;
            }
            continue;
        }
        match command {
            P2pCommand::BroadcastBlock(block) => {
                // Mirror of the inbound transport check: the sender must lead somewhere in
                // the retained schedule; every importer applies the exact
                // `producer == leader_for(anchor)` fence. Recipients are all other manifest
                // members — during a scheduled handoff the incoming leader must keep
                // receiving live blocks.
                if !leadership.is_scheduled_leader(&local_ed25519_public_key) {
                    metrics::counter!("zone_p2p_role_invalid_messages_dropped_total").increment(1);
                    warn!(target: "zone::p2p", "Ignoring live block broadcast command without retained scheduled leadership");
                    continue;
                }

                if block.len() > MAX_MESSAGE_SIZE as usize {
                    error!(target: "zone::p2p", block_size_bytes = block.len(), max_message_size_bytes = MAX_MESSAGE_SIZE, "Canonical block exceeds the P2P message size limit; block was not broadcast");
                    continue;
                }

                let recipients = &others;
                let sent = tokio::time::timeout(BROADCAST_RETRY_TIMEOUT, async {
                    loop {
                        let sent = senders.blocks
                            .send(Recipients::Some(recipients.clone()), block.clone(), true)
                            .await
                            .map_err(|err| eyre::eyre!("failed broadcasting zone block: {err}"))?;
                        if !sent.is_empty() || recipients.is_empty() {
                            return Ok::<_, eyre::Report>(sent);
                        }
                        debug!(target: "zone::p2p", "No peers are connected; retrying canonical block broadcast");
                        tokio::time::sleep(BROADCAST_RETRY_INTERVAL).await;
                    }
                }).await;
                let sent = match sent {
                    Ok(sent) => sent?,
                    Err(_) => {
                        warn!(target: "zone::p2p", timeout_secs = BROADCAST_RETRY_TIMEOUT.as_secs(), "No peers connected before block broadcast timed out");
                        continue;
                    }
                };
                if sent.len() != recipients.len() {
                    debug!(target: "zone::p2p", connected = sent.len(), configured = recipients.len(), "Some peers are not connected; block was not sent to them");
                }
            }

            P2pCommand::BroadcastSettlementProposal(proposal) => {
                if !leadership.is_scheduled_leader(&local_ed25519_public_key) {
                    metrics::counter!("zone_p2p_role_invalid_messages_dropped_total").increment(1);
                    warn!(target: "zone::p2p", "Ignoring settlement proposal command without retained scheduled leadership");
                    continue;
                }
                // Only quorum members sign, so only they are asked. An RPC-only standby that
                // received a proposal would have nothing to answer it with.
                senders
                    .settlement_proposals
                    .send(
                        Recipients::Some(leadership.quorum_peers(&others)),
                        proposal,
                        true,
                    )
                    .await
                    .wrap_err("failed broadcasting settlement proposal")?;
            }

            P2pCommand::SendSettlementSignature { leader, signature } => {
                if !leadership.is_quorum_member(&local_ed25519_public_key) {
                    metrics::counter!("zone_p2p_role_invalid_messages_dropped_total").increment(1);
                    warn!(target: "zone::p2p", "Ignoring settlement signature command from a node outside the on-chain quorum");
                    continue;
                }
                // The signature answers a specific proposal, so it returns to that
                // proposal's sender (not to the most recent leader. Important during handoff)
                if leader == local_ed25519_public_key || !leadership.is_scheduled_leader(&leader) {
                    metrics::counter!("zone_p2p_role_invalid_messages_dropped_total").increment(1);
                    warn!(target: "zone::p2p", %leader, "Ignoring settlement signature addressed to a peer without retained scheduled leadership");
                    continue;
                }
                senders
                    .settlement_signatures
                    .send(Recipients::Some(vec![leader]), signature, true)
                    .await
                    .wrap_err("failed sending settlement signature")?;
            }

            P2pCommand::RequestBackfill { .. } => {
                unreachable!("backfill requests are routed before role derivation")
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
                let mut frame = Vec::with_capacity(9 + PeerTip::ENCODED_LEN);
                frame.push(BACKFILL_COMPLETE_FRAME);
                frame.extend_from_slice(&request_id.to_be_bytes());
                tip.encode_into(&mut frame);
                senders
                    .backfill_responses
                    .send(Recipients::Some(vec![peer]), frame, true)
                    .await
                    .map_err(|err| eyre::eyre!("failed completing block backfill: {err}"))?;
            }

            P2pCommand::ForwardTransaction {
                transaction_hash,
                transaction,
            } => {
                // Only followers run the transaction-forwarding task. Keep the outbound role
                // fence, but send to every other quorum member so every possible successor
                // retains the transaction before a leadership handoff. RPC-only standbys can
                // originate transactions but never need to retain transactions from other nodes.
                let Some(record) = leadership.next_anchor_record() else {
                    metrics::counter!("zone_p2p_uninitialized_leadership_commands_dropped_total")
                        .increment(1);
                    warn!(target: "zone::p2p", ?transaction_hash, "Dropping forwarded transaction while leadership is uninitialized");
                    continue;
                };
                let leader = record.leader;
                if leader == local_ed25519_public_key {
                    metrics::counter!("zone_p2p_role_invalid_messages_dropped_total").increment(1);
                    warn!(target: "zone::p2p", ?transaction_hash, "Ignoring outbound transaction command on the next-anchor leader");
                    continue;
                }
                let recipients = leadership.quorum_peers(&others);
                let configured = recipients.len();
                let transaction_size = transaction.len();
                let sent = match senders
                    .transactions
                    .send(Recipients::Some(recipients), transaction, false)
                    .await
                {
                    Ok(sent) => sent,
                    Err(err) => {
                        metrics::counter!("zone_p2p_transaction_sends_without_peers_total")
                            .increment(1);
                        warn!(target: "zone::p2p", ?transaction_hash, transaction_size_bytes = transaction_size, %err, "Failed to forward transaction to quorum peers; dropping this send attempt");
                        continue;
                    }
                };
                if sent.is_empty() {
                    metrics::counter!("zone_p2p_transaction_sends_without_peers_total")
                        .increment(1);
                    warn!(target: "zone::p2p", ?transaction_hash, configured, transaction_size_bytes = transaction_size, "Forwarded transaction reached no quorum peer (peers disconnected, sender throttled, or outbound queue full); dropping this send attempt");
                } else {
                    debug!(target: "zone::p2p", ?transaction_hash, connected = sent.len(), configured, transaction_size_bytes = transaction_size, "Forwarded transaction to quorum peers");
                }
            }
        }
    }

    Err(eyre::eyre!("P2P command channel closed unexpectedly"))
}

async fn run_receivers(
    local_ed25519_public_key: PublicKey,
    leadership: LeadershipSchedule,
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
        mut transactions,
    } = receivers;

    loop {
        let event = tokio::select! {
            // Got a block
            result = blocks.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("block channel receive failed: {err}"))?;
                // A lagging follower must not drop the rightful
                // producer of in-between anchors just because a later transition is already
                // the "current" record.
                if peer == local_ed25519_public_key
                    || !leadership.is_scheduled_leader(&peer)
                {
                    warn!(target: "zone::p2p", %peer, "Ignoring live block from non-leader");
                    continue;
                }
                P2pEvent::BlockReceived { leader_ed25519_public_key: peer, block: bytes.into() }
            }

            // Got a settlement proposal at a batch boundary
            result = settlement_proposals.recv() => {
                let (peer, bytes) = result.wrap_err("settlement proposal channel receive failed")?;
                // The proposer must lead somewhere in the retained schedule — during a scheduled handoff the
                // outgoing leader still settles pre-boundary batches. The follower rebuilds
                // the proposal from its own state before signing. An RPC-only member drops the
                // proposal here: only the on-chain quorum signs.
                if peer == local_ed25519_public_key
                    || !leadership.is_scheduled_leader(&peer)
                    || !leadership.is_quorum_member(&local_ed25519_public_key)
                {
                    warn!(target: "zone::p2p", %peer, "Ignoring settlement proposal from ineligible peer");
                    continue;
                }
                P2pEvent::SettlementProposalReceived { leader: peer, proposal: bytes.into() }
            }

            // Got a response from a follower to the settlement proposal
            result = settlement_signatures.recv() => {
                let (peer, bytes) = result.wrap_err("settlement signature channel receive failed")?;
                // An RPC-only member has no address registered with `ZonePortal`, so its
                // signature could never be counted; reject it at the transport instead of
                // relying on the attestation-address lookup further in.
                if peer == local_ed25519_public_key
                    || !leadership.is_quorum_member(&peer)
                    || !leadership.is_scheduled_leader(&local_ed25519_public_key)
                {
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
                // Backfill responses are accepted from any quorum member — the request side
                // only ever asks them, so a page from an internet-facing standby is unsolicited.
                if peer == local_ed25519_public_key
                    || !manifest.contains_ed25519_public_key(&peer)
                    || !leadership.is_quorum_member(&peer)
                {
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
                        let Some(tip) = PeerTip::decode(payload) else {
                            warn!(target: "zone::p2p", %peer, request_id, size = payload.len(), "Ignoring malformed backfill completion");
                            continue;
                        };
                        P2pEvent::BackfillCompleted { peer, tip }
                    }
                    _ => {
                        warn!(target: "zone::p2p", %peer, frame_kind, "Ignoring backfill response with unknown frame kind");
                        continue;
                    }
                }
            }

            // Got a transaction forwarded by an authenticated manifest peer. Only quorum members
            // admit these into their pools; RPC-only standbys can never become leader.
            result = transactions.recv() => {
                let (peer, bytes) = result.map_err(|err| eyre::eyre!("transaction channel receive failed: {err}"))?;
                if peer == local_ed25519_public_key
                    || !manifest.contains_ed25519_public_key(&peer)
                    || !leadership.is_quorum_member(&local_ed25519_public_key)
                {
                    metrics::counter!("zone_p2p_role_invalid_messages_dropped_total").increment(1);
                    warn!(target: "zone::p2p", %peer, "Ignoring transaction from role-invalid peer");
                    continue;
                }
                metrics::counter!("zone_p2p_transactions_received_total").increment(1);
                P2pEvent::TransactionReceived {
                    follower_ed25519_public_key: peer,
                    transaction: bytes.into(),
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

    use alloy_primitives::{B256, address};
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

    fn test_tip(zone_height: u64) -> super::PeerTip {
        super::PeerTip {
            zone_height,
            zone_hash: B256::with_last_byte(zone_height as u8),
            tempo_block_number: zone_height + 1000,
            tempo_block_hash: B256::with_last_byte((zone_height + 1) as u8),
        }
    }

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
    fn leader_counts_as_unresponsive_only_after_the_response_timeout() {
        // Drives the leader-only-until-it-stops-answering choice in the backfill request arm:
        // a served request must not widen the source set, an abandoned one must.
        let leader = ed25519_identity(1).ed25519_public_key();
        let now = Instant::now();
        let mut lifecycle = BackfillJob::default();

        // Nothing outstanding: the leader is the sole source.
        assert!(!lifecycle.is_unresponsive(&leader, now));

        let (request_id, peers) = lifecycle
            .begin_request(std::slice::from_ref(&leader), now)
            .unwrap();
        lifecycle.finish_send(request_id, &peers);
        // Still within the window, so it is being served, not stalling.
        assert!(!lifecycle.is_unresponsive(&leader, now + BACKFILL_RESPONSE_TIMEOUT / 2));
        // Past the window the node must be free to ask the rest of the quorum.
        assert!(lifecycle.is_unresponsive(&leader, now + BACKFILL_RESPONSE_TIMEOUT));

        // A completed page clears the state, returning to leader-only.
        assert!(lifecycle.complete(&leader, request_id, now));
        assert!(!lifecycle.is_unresponsive(&leader, now + BACKFILL_RESPONSE_TIMEOUT));
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

    /// Resend `command` until the test drops the returned handle.
    ///
    /// Commonware drops messages for peers that have not handshaked yet, so a phase that must be
    /// observed repeats its command until the expected peer sees it.
    fn repeat(
        commands: tokio::sync::mpsc::Sender<P2pCommand>,
        command: P2pCommand,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                commands.send(command.clone()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    }

    /// Drain events for `duration`, failing if any of them satisfies `forbidden`.
    async fn assert_no_event_matching(
        events: &mut tokio::sync::mpsc::Receiver<P2pEvent>,
        duration: Duration,
        context: &str,
        forbidden: impl Fn(&P2pEvent) -> bool,
    ) {
        let result = tokio::time::timeout(duration, async {
            while let Some(event) = events.recv().await {
                assert!(!forbidden(&event), "{context}");
            }
        })
        .await;
        assert!(result.is_err(), "{context}: event channel closed");
    }

    async fn assert_no_backfill_response_events(
        events: &mut tokio::sync::mpsc::Receiver<P2pEvent>,
        duration: Duration,
        context: &str,
    ) {
        assert_no_event_matching(events, duration, context, |event| {
            matches!(
                event,
                P2pEvent::BackfillBlockReceived { .. } | P2pEvent::BackfillCompleted { .. }
            )
        })
        .await;
    }

    /// Manifest TOML for a topology with one `rpc_only` standby.
    ///
    /// Node `index` gets `secp256k1_identity(seed_base + index)`, except the standby, which
    /// declares no address at all — the shape the loader requires.
    fn manifest_with_standby(
        identities: &[Ed25519Identity],
        addresses: &[SocketAddr],
        seed_base: u64,
        standby: usize,
    ) -> String {
        let mut input = format!(
            "zone_id = 9\nleader_ed25519_public_key = \"{}\"\n",
            const_hex::encode_prefixed(identities[0].ed25519_public_key().as_ref())
        );
        for (index, (identity, address)) in identities.iter().zip(addresses).enumerate() {
            let rpc_only = index == standby;
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\naddress = \"{address}\"\nrpc_only = {rpc_only}\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref()),
            ));
            if !rpc_only {
                input.push_str(&format!(
                    "secp256k1_address = \"{}\"\n",
                    secp256k1_identity(seed_base + index as u64).address(),
                ));
            }
        }
        input
    }

    #[test]
    fn attestation_addresses_and_peer_sets_exclude_rpc_only_members() {
        let identities = [41_u64, 42, 43, 44].map(ed25519_identity);
        let addresses: Vec<SocketAddr> = (0..4)
            .map(|index| format!("127.0.0.1:{}", 9200 + index).parse().unwrap())
            .collect();
        let input = manifest_with_standby(&identities, &addresses, 41, 3);
        let manifest = Arc::new(ZoneManifest::parse(&input).unwrap());
        let config = P2pConfig {
            manifest: manifest.clone(),
            ed25519_identity: ed25519_identity(41),
            secp256k1_identity: Some(secp256k1_identity(41)),
            listen: available_address(),
            bypass_ip_check: false,
            leadership: manifest.leadership_schedule(),
        };

        // The standby has no registered address, so a signature claiming to be its has nothing
        // to match against and the leader cannot count it.
        let addresses = config.block_attestation_addresses();
        assert_eq!(addresses.len(), 3);
        assert!(!addresses.contains_key(&identities[3].ed25519_public_key()));

        let peers = identities
            .iter()
            .map(|identity| identity.ed25519_public_key())
            .collect::<Vec<_>>();
        let [leader, follower_a, follower_b, rpc] = peers.clone().try_into().unwrap();
        assert_eq!(
            config.leadership().quorum_peers(&peers),
            vec![leader, follower_a, follower_b]
        );
        assert!(config.leadership().is_rpc_only(&rpc));
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
                manifest
                    .validate_node(
                        9,
                        &identity.ed25519_public_key(),
                        Some(secp256k1_identity.address()),
                        None,
                    )
                    .unwrap();
                spawn_p2p(
                    P2pConfig {
                        manifest: manifest.clone(),
                        ed25519_identity: identity,
                        secp256k1_identity: Some(secp256k1_identity),
                        listen,
                        bypass_ip_check: false,
                        leadership: crate::LeadershipSchedule::seeded(
                            manifest.bootstrap_leadership(),
                        ),
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
        let broadcaster = repeat(commands, P2pCommand::BroadcastBlock(block.clone()));

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
        // Role-invalid commands are dropped without stopping either runtime.
        leader_commands
            .send(P2pCommand::ForwardTransaction {
                transaction_hash: B256::with_last_byte(1),
                transaction: vec![0x01],
            })
            .await
            .unwrap();
        follower_commands
            .send(P2pCommand::BroadcastBlock(block.clone()))
            .await
            .unwrap();

        let transaction_hash = B256::with_last_byte(2);
        let transaction = vec![0x76, 0x01, 0x02, 0x03];
        let forwarder = repeat(
            follower_commands.clone(),
            P2pCommand::ForwardTransaction {
                transaction_hash,
                transaction: transaction.clone(),
            },
        );
        for index in [0, 2] {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(P2pEvent::TransactionReceived {
                        follower_ed25519_public_key,
                        transaction: received,
                    }) = handles[index].events_mut().recv().await
                    {
                        assert_eq!(follower_ed25519_public_key, first_follower_peer);
                        assert_eq!(received, transaction);
                        return;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("node-{index} did not receive forwarded transaction"));
        }
        forwarder.abort();

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
            .send(P2pCommand::SendSettlementSignature {
                leader: leader_peer.clone(),
                signature: settlement_signature.clone(),
            })
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
                tip: test_tip(99),
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
                tip: test_tip(100),
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
                tip: test_tip(9),
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
                    Some(P2pEvent::BackfillCompleted { tip, .. }) if tip.zone_height == 9 => {
                        assert_eq!(tip, test_tip(9));
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
                tip: test_tip(10),
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
                tip: test_tip(12),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::BackfillCompleted { peer, tip }) =
                    handles[0].events_mut().recv().await
                    && tip == test_tip(12)
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

    /// A finalized transition observed ahead of its activation boundary must not reroute
    /// traffic early: while the local applied anchor is still governed by the outgoing
    /// leader, its block broadcasts and settlement proposals keep flowing (including to the
    /// incoming leader), signatures return to the proposer, and forwarded transactions
    /// reach every replica before the transition activates.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn advance_scheduled_transition_keeps_anchor_relevant_routing() {
        let addresses = [
            available_address(),
            available_address(),
            available_address(),
        ];
        let identities = [
            ed25519_identity(21),
            ed25519_identity(22),
            ed25519_identity(23),
        ];
        let mut input = format!(
            "zone_id = 9\nleader_ed25519_public_key = \"{}\"\n",
            const_hex::encode_prefixed(identities[0].ed25519_public_key().as_ref())
        );
        for (index, (identity, address)) in identities.iter().zip(addresses).enumerate() {
            let secp256k1_identity = secp256k1_identity(index as u64 + 21);
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"{}\"\naddress = \"{address}\"\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref()),
                secp256k1_identity.address(),
            ));
        }
        let manifest = Arc::new(ZoneManifest::parse(&input).unwrap());
        let outgoing_leader = identities[0].ed25519_public_key();
        let incoming_leader = identities[1].ed25519_public_key();
        let follower_peer = identities[2].ed25519_public_key();

        let mut handles = identities
            .into_iter()
            .zip(addresses)
            .enumerate()
            .map(|(index, (identity, listen))| {
                // Every node has observed the A→B transition at a future activation while
                // its applied anchor is still before the boundary: A (node 0) remains the
                // producer of the next anchor.
                let leadership = crate::LeadershipSchedule::seeded(manifest.bootstrap_leadership());
                leadership
                    .publish(crate::LeadershipState::new(1, incoming_leader.clone(), 100))
                    .unwrap();
                leadership.record_applied_anchor(10);
                spawn_p2p(
                    P2pConfig {
                        manifest: manifest.clone(),
                        ed25519_identity: identity,
                        secp256k1_identity: Some(secp256k1_identity(index as u64 + 21)),
                        listen,
                        bypass_ip_check: false,
                        leadership,
                    },
                    P2pNetworkId::new(1, address!("1111111111111111111111111111111111111111")),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();

        // The outgoing leader's live blocks reach every other member — including the
        // incoming leader, which must keep importing until the boundary.
        let block = vec![0xf8, 0x01, 0x80];
        let outgoing_commands = handles[0].parts.as_ref().unwrap().commands.clone();
        let broadcast_block = block.clone();
        let broadcaster = tokio::spawn(async move {
            loop {
                outgoing_commands
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
                        leader_ed25519_public_key,
                        block: received,
                    }) = handle.events_mut().recv().await
                    {
                        assert_eq!(leader_ed25519_public_key, outgoing_leader);
                        assert_eq!(received, block);
                        return;
                    }
                }
            })
            .await
            .expect("peer did not receive the outgoing leader's live block");
        }
        broadcaster.abort();

        let outgoing_commands = handles[0].parts.as_ref().unwrap().commands.clone();
        let incoming_commands = handles[1].parts.as_ref().unwrap().commands.clone();
        let follower_commands = handles[2].parts.as_ref().unwrap().commands.clone();

        // The outgoing leader still settles pre-boundary batches: its proposal is accepted
        // and the signature returns to it, not to the incoming leader.
        let proposal = vec![0x10, 0x20];
        outgoing_commands
            .send(P2pCommand::BroadcastSettlementProposal(proposal.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::SettlementProposalReceived {
                    leader,
                    proposal: received,
                }) = handles[2].events_mut().recv().await
                {
                    assert_eq!(leader, outgoing_leader);
                    assert_eq!(received, proposal);
                    return;
                }
            }
        })
        .await
        .expect("follower did not receive the outgoing leader's settlement proposal");

        let signature = vec![0x30, 0x40];
        follower_commands
            .send(P2pCommand::SendSettlementSignature {
                leader: outgoing_leader.clone(),
                signature: signature.clone(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::SettlementSignatureReceived {
                    follower,
                    signature: received,
                }) = handles[0].events_mut().recv().await
                {
                    assert_eq!(follower, follower_peer);
                    assert_eq!(received, signature);
                    return;
                }
            }
        })
        .await
        .expect("outgoing leader did not receive the settlement signature");

        // Forwarded transactions reach every other replica. In particular, B retains C's
        // transaction before its leadership activates, while B's own forward reaches A and C.
        let transaction = vec![0x76, 0x01, 0x02, 0x03];
        let follower_forwarder = repeat(
            follower_commands.clone(),
            P2pCommand::ForwardTransaction {
                transaction_hash: B256::with_last_byte(1),
                transaction: transaction.clone(),
            },
        );
        let incoming_forwarder = repeat(
            incoming_commands.clone(),
            P2pCommand::ForwardTransaction {
                transaction_hash: B256::with_last_byte(2),
                transaction: transaction.clone(),
            },
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut received_from_incoming = false;
            let mut received_from_follower = false;
            loop {
                if let Some(P2pEvent::TransactionReceived {
                    follower_ed25519_public_key,
                    transaction: received,
                }) = handles[0].events_mut().recv().await
                {
                    assert_eq!(received, transaction);
                    if follower_ed25519_public_key == incoming_leader {
                        received_from_incoming = true;
                    } else if follower_ed25519_public_key == follower_peer {
                        received_from_follower = true;
                    }
                    if received_from_incoming && received_from_follower {
                        return;
                    }
                }
            }
        })
        .await
        .expect("outgoing leader did not receive both forwarded transactions");

        for (recipient, expected_sender) in
            [(1, follower_peer.clone()), (2, incoming_leader.clone())]
        {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(P2pEvent::TransactionReceived {
                        follower_ed25519_public_key,
                        transaction: received,
                    }) = handles[recipient].events_mut().recv().await
                    {
                        assert_eq!(follower_ed25519_public_key, expected_sender);
                        assert_eq!(received, transaction);
                        return;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!("node-{recipient} did not receive the peer's forwarded transaction")
            });
        }
        follower_forwarder.abort();
        incoming_forwarder.abort();

        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
                .await
                .expect("P2P runtime did not stop")
                .expect("P2P runtime failed");
        }
    }

    /// An RPC-only member replicates blocks and forwards its own transactions into the quorum,
    /// but receives neither settlement traffic nor transactions submitted to other nodes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_only_follower_replicates_but_never_settles() {
        const LEADER: usize = 0;
        const QUORUM_FOLLOWER: usize = 1;
        const QUORUM_FOLLOWER_B: usize = 2;
        const RPC_FOLLOWER: usize = 3;

        let addresses = [
            available_address(),
            available_address(),
            available_address(),
            available_address(),
        ];
        let identities = [31_u64, 32, 33, 34].map(ed25519_identity);
        let input = manifest_with_standby(&identities, &addresses, 31, RPC_FOLLOWER);
        let manifest = Arc::new(ZoneManifest::parse(&input).unwrap());
        let leader_peer = identities[LEADER].ed25519_public_key();
        let rpc_follower_peer = identities[RPC_FOLLOWER].ed25519_public_key();
        let mut handles = identities
            .into_iter()
            .zip(addresses)
            .enumerate()
            .map(|(index, (identity, listen))| {
                let leadership = manifest.leadership_schedule();
                leadership.publish(manifest.bootstrap_leadership()).unwrap();
                spawn_p2p(
                    P2pConfig {
                        manifest: manifest.clone(),
                        ed25519_identity: identity,
                        // The standby is provisioned without quorum key material at all.
                        secp256k1_identity: (index != RPC_FOLLOWER)
                            .then(|| secp256k1_identity(index as u64 + 31)),
                        listen,
                        bypass_ip_check: false,
                        leadership,
                    },
                    P2pNetworkId::new(1, address!("1111111111111111111111111111111111111111")),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let leader_commands = handles[LEADER].parts.as_ref().unwrap().commands.clone();
        let quorum_follower_commands = handles[QUORUM_FOLLOWER]
            .parts
            .as_ref()
            .unwrap()
            .commands
            .clone();
        let rpc_commands = handles[RPC_FOLLOWER]
            .parts
            .as_ref()
            .unwrap()
            .commands
            .clone();

        // Replication reaches every replica, including the RPC standby: it serves reads from its
        // own imported chain. Waiting for all three also establishes the full mesh.
        let block = vec![0xf8, 0x01, 0x80];
        let broadcaster = repeat(
            leader_commands.clone(),
            P2pCommand::BroadcastBlock(block.clone()),
        );
        for index in [QUORUM_FOLLOWER, QUORUM_FOLLOWER_B, RPC_FOLLOWER] {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(P2pEvent::BlockReceived {
                        leader_ed25519_public_key,
                        block: received,
                    }) = handles[index].events_mut().recv().await
                    {
                        assert_eq!(leader_ed25519_public_key, leader_peer);
                        assert_eq!(received, block);
                        return;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("node-{index} did not receive the leader's block"));
        }
        broadcaster.abort();

        // Settlement proposals go only to the quorum.
        let proposal = vec![0x10, 0x20];
        let proposer = repeat(
            leader_commands.clone(),
            P2pCommand::BroadcastSettlementProposal(proposal.clone()),
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::SettlementProposalReceived {
                    proposal: received, ..
                }) = handles[QUORUM_FOLLOWER].events_mut().recv().await
                {
                    assert_eq!(received, proposal);
                    return;
                }
            }
        })
        .await
        .expect("quorum follower did not receive the settlement proposal");
        // Proposals keep flowing through this window, so the standby has every chance to leak one.
        assert_no_event_matching(
            handles[RPC_FOLLOWER].events_mut(),
            Duration::from_secs(2),
            "RPC-only follower was asked to sign a settlement",
            |event| matches!(event, P2pEvent::SettlementProposalReceived { .. }),
        )
        .await;
        proposer.abort();

        // ...and a signature from outside the quorum never reaches the leader: the command is
        // dropped before it is sent, and the leader's receive guard would reject it anyway.
        rpc_commands
            .send(P2pCommand::SendSettlementSignature {
                leader: leader_peer.clone(),
                signature: vec![0x30, 0x40],
            })
            .await
            .unwrap();

        // Operator RPC submissions reach every quorum member, not just the active leader.
        let transaction_hash = B256::with_last_byte(7);
        let transaction = vec![0x76, 0x07];
        let forwarder = repeat(
            rpc_commands.clone(),
            P2pCommand::ForwardTransaction {
                transaction_hash,
                transaction: transaction.clone(),
            },
        );
        for index in [LEADER, QUORUM_FOLLOWER, QUORUM_FOLLOWER_B] {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(P2pEvent::TransactionReceived {
                        follower_ed25519_public_key,
                        transaction: received,
                    }) = handles[index].events_mut().recv().await
                    {
                        assert_eq!(follower_ed25519_public_key, rpc_follower_peer);
                        assert_eq!(received, transaction);
                        return;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!("node-{index} did not receive the RPC-only follower's transaction")
            });
        }
        forwarder.abort();

        // Transactions submitted to a quorum follower stay within the quorum. The RPC-only
        // standby continues replicating blocks but does not retain unrelated pending bodies.
        let quorum_forwarder = repeat(
            quorum_follower_commands,
            P2pCommand::ForwardTransaction {
                transaction_hash: B256::with_last_byte(8),
                transaction: vec![0x76, 0x08],
            },
        );
        assert_no_event_matching(
            handles[RPC_FOLLOWER].events_mut(),
            Duration::from_secs(2),
            "RPC-only follower received another node's transaction",
            |event| matches!(event, P2pEvent::TransactionReceived { .. }),
        )
        .await;
        quorum_forwarder.abort();

        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
                .await
                .expect("P2P runtime did not stop")
                .expect("P2P runtime failed");
        }
    }

    /// Catch-up must reach a reachable quorum follower while the leader is offline.
    ///
    /// The leader is preferred as the sole source, but a leader that is not connected never
    /// receives a request, so its outstanding entry is cleared on every attempt and
    /// `is_unresponsive` never fires. Without widening in the same pass the node would re-pick
    /// the unreachable leader forever and stay stuck for the whole outage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_reaches_a_quorum_follower_while_the_leader_is_offline() {
        let addresses = [
            available_address(),
            available_address(),
            available_address(),
        ];
        let identities = [51_u64, 52, 53].map(ed25519_identity);
        let mut input = format!(
            "zone_id = 9\nleader_ed25519_public_key = \"{}\"\n",
            const_hex::encode_prefixed(identities[0].ed25519_public_key().as_ref())
        );
        for (index, (identity, address)) in identities.iter().zip(addresses).enumerate() {
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"{}\"\naddress = \"{address}\"\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref()),
                secp256k1_identity(index as u64 + 51).address(),
            ));
        }
        let manifest = Arc::new(ZoneManifest::parse(&input).unwrap());

        // The leader (node 0) is never spawned, so it is a configured peer that never connects.
        let mut handles = [1_usize, 2]
            .map(|index| {
                spawn_p2p(
                    P2pConfig {
                        manifest: manifest.clone(),
                        ed25519_identity: ed25519_identity(index as u64 + 51),
                        secp256k1_identity: Some(secp256k1_identity(index as u64 + 51)),
                        listen: addresses[index],
                        bypass_ip_check: false,
                        leadership: crate::LeadershipSchedule::seeded(
                            manifest.bootstrap_leadership(),
                        ),
                    },
                    P2pNetworkId::new(1, address!("1111111111111111111111111111111111111111")),
                )
                .unwrap()
            })
            .into_iter()
            .collect::<Vec<_>>();

        let requester_commands = handles[0].parts.as_ref().unwrap().commands.clone();
        let requester = repeat(requester_commands, P2pCommand::RequestBackfill { start: 1 });
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(P2pEvent::BackfillRequested { peer, start, .. }) =
                    handles[1].events_mut().recv().await
                {
                    assert_eq!(peer, identities[1].ed25519_public_key());
                    assert_eq!(start, 1);
                    return;
                }
            }
        })
        .await
        .expect("catch-up never widened past the offline leader to a reachable quorum follower");
        requester.abort();

        for handle in handles {
            tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
                .await
                .expect("P2P runtime did not stop")
                .expect("P2P runtime failed");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn followers_exchange_transactions_while_leader_is_offline() {
        let addresses = [
            available_address(),
            available_address(),
            available_address(),
        ];
        let identities = [
            ed25519_identity(11),
            ed25519_identity(12),
            ed25519_identity(13),
        ];
        let mut input = format!(
            "zone_id = 9\nleader_ed25519_public_key = \"{}\"\n",
            const_hex::encode_prefixed(identities[0].ed25519_public_key().as_ref())
        );
        for (index, (identity, address)) in identities.iter().zip(addresses).enumerate() {
            let secp256k1_identity = secp256k1_identity(index as u64 + 11);
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{index}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"{}\"\naddress = \"{address}\"\n",
                const_hex::encode_prefixed(identity.ed25519_public_key().as_ref()),
                secp256k1_identity.address(),
            ));
        }
        let manifest = Arc::new(ZoneManifest::parse(&input).unwrap());
        let sender_peer = identities[1].ed25519_public_key();
        let mut handles = identities
            .into_iter()
            .zip(addresses)
            .enumerate()
            .skip(1)
            .map(|(index, (identity, listen))| {
                let secp256k1_identity = secp256k1_identity(index as u64 + 11);
                let role = manifest
                    .validate_node(
                        9,
                        &identity.ed25519_public_key(),
                        Some(secp256k1_identity.address()),
                        None,
                    )
                    .unwrap();
                assert_eq!(role, crate::Role::Follower);
                spawn_p2p(
                    P2pConfig {
                        manifest: manifest.clone(),
                        ed25519_identity: identity,
                        secp256k1_identity: Some(secp256k1_identity),
                        listen,
                        bypass_ip_check: false,
                        leadership: crate::LeadershipSchedule::seeded(
                            manifest.bootstrap_leadership(),
                        ),
                    },
                    P2pNetworkId::new(1, address!("1111111111111111111111111111111111111111")),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();

        for handle in &mut handles {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !matches!(
                    handle.events_mut().recv().await,
                    Some(P2pEvent::Started { .. })
                ) {}
            })
            .await
            .expect("follower P2P runtime did not start");
        }
        let commands = handles[0].parts.as_ref().unwrap().commands.clone();
        let transaction = vec![0x76, 0x01];
        let forwarder = repeat(
            commands,
            P2pCommand::ForwardTransaction {
                transaction_hash: B256::with_last_byte(1),
                transaction: transaction.clone(),
            },
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(P2pEvent::TransactionReceived {
                    follower_ed25519_public_key,
                    transaction: received,
                }) = handles[1].events_mut().recv().await
                {
                    assert_eq!(follower_ed25519_public_key, sender_peer);
                    assert_eq!(received, transaction);
                    return;
                }
            }
        })
        .await
        .expect("reachable follower did not receive transaction while leader was offline");
        forwarder.abort();

        for handle in handles {
            handle.shutdown().await.unwrap();
        }
    }
}
