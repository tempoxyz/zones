//! Dedicated encrypted inter-Zone request/response transport.
//!
//! This network is deliberately separate from intra-Zone replication. Static Ed25519 keys and
//! endpoints only let Commonware route an encrypted connection; an installed, finalized ECDSA
//! authority set and a two-sided signed session transcript authorize service traffic.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, keccak256};
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use commonware_cryptography::{
    Signer as _,
    ed25519::{PrivateKey, PublicKey},
};
use commonware_p2p::{
    AddressableManager as _, AddressableTrackedPeers, Receiver as _, Recipients, Sender as _,
    authenticated::lookup,
};
use commonware_runtime::{IoBuf, Quota, Supervisor as _};
use commonware_utils::{NZU32, ordered::Map};
use rand::{RngCore as _, rngs::OsRng};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;
use zone_fast_transfer::{AuthenticatedPeerSession, EpochRoster};
use zone_primitives::fast_transfer::{
    CanonicalEncode, MAX_SERVICE_ENVELOPE_BYTES, SignatureBytes, TransportSessionProof, ZoneDomain,
};

use crate::{ManifestAddress, network};

const INTER_ZONE_REQUEST_CHANNEL: u64 = 0;
const INTER_ZONE_RESPONSE_CHANNEL: u64 = 1;
const INTER_ZONE_NAMESPACE: &[u8] = b"TEMPO_ZONE_PRIVATE_INTER_ZONE_T14_V1";
const ROUTING_PEERS: usize = 30;
const ROUTING_ZONES: usize = 10;
const PEERS_PER_ZONE: usize = 3;
const PORT_BACKLOG: usize = 128;
const SESSION_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const SESSION_REPLAY_WINDOW: Duration = Duration::from_secs(10 * 60);
const WIRE_VERSION: u8 = 1;
const CHALLENGE_TAG: u8 = 1;
const DELIVERY_TAG: u8 = 2;
const CHALLENGE_RESPONSE_TAG: u8 = 3;
const ACK_TAG: u8 = 4;
const ERROR_TAG: u8 = 5;

/// Maximum application frame accepted by the dedicated Commonware network.
pub const MAX_INTER_ZONE_MESSAGE_SIZE: u32 = 16 * 1024;

type CommonwareSender = lookup::Sender<PublicKey, commonware_runtime::tokio::Context>;
type CommonwareReceiver = lookup::Receiver<PublicKey>;

/// One configured Commonware identity and endpoint. This record is routing data, never roster
/// authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterZoneRoutingPeer {
    pub zone_id: u32,
    pub ed25519: PublicKey,
    pub endpoint: ManifestAddress,
}

/// Exact ten-Zone routing topology for the private C4 carrier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterZoneRoutingConfig {
    pub l1_chain_id: u64,
    pub local_zone_id: u32,
    pub listen: SocketAddr,
    pub bypass_ip_check: bool,
    pub peers: Vec<InterZoneRoutingPeer>,
}

impl InterZoneRoutingConfig {
    pub fn validate(&self, local: &PublicKey, intra_zone_listen: SocketAddr) -> eyre::Result<()> {
        eyre::ensure!(
            self.listen != intra_zone_listen,
            "inter-Zone Commonware requires a dedicated listen address"
        );
        eyre::ensure!(
            self.peers.len() == ROUTING_PEERS,
            "inter-Zone routing must contain exactly {ROUTING_PEERS} replicas"
        );
        let identities = self
            .peers
            .iter()
            .map(|peer| peer.ed25519.clone())
            .collect::<BTreeSet<_>>();
        eyre::ensure!(
            identities.len() == ROUTING_PEERS,
            "inter-Zone routing contains duplicate Ed25519 identities"
        );
        let mut zones = BTreeMap::<u32, usize>::new();
        for peer in &self.peers {
            *zones.entry(peer.zone_id).or_default() += 1;
        }
        eyre::ensure!(
            zones.len() == ROUTING_ZONES && zones.values().all(|count| *count == PEERS_PER_ZONE),
            "inter-Zone routing must contain exactly three replicas for each of ten Zones"
        );
        eyre::ensure!(
            self.peers
                .iter()
                .any(|peer| peer.zone_id == self.local_zone_id && &peer.ed25519 == local),
            "local Commonware identity is absent from its inter-Zone routing group"
        );
        if self.peers.iter().any(|peer| peer.endpoint.is_dns()) {
            eyre::ensure!(
                self.bypass_ip_check,
                "DNS inter-Zone endpoints require explicit source-IP filtering bypass"
            );
        }
        Ok(())
    }
}

/// Finalized ECDSA authority associated with one configured Commonware identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterZonePeerAuthority {
    pub domain: ZoneDomain,
    pub certificate_member: Address,
}

/// Exact finalized authority installed after importing the anchor. The routing topology is checked
/// for a one-to-one Ed25519-to-ECDSA mapping, but routing data alone never creates authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterZoneAuthoritySet {
    pub rosters: BTreeMap<u32, EpochRoster>,
    pub peers: BTreeMap<PublicKey, InterZonePeerAuthority>,
}

impl InterZoneAuthoritySet {
    fn validate(
        &self,
        routing: &InterZoneRoutingConfig,
        local_ed25519: &PublicKey,
        local_ecdsa: Address,
    ) -> eyre::Result<()> {
        eyre::ensure!(
            self.rosters.len() == ROUTING_ZONES && self.peers.len() == ROUTING_PEERS,
            "inter-Zone authority must contain ten rosters and thirty peer bindings"
        );
        for route in &routing.peers {
            let authority = self.peers.get(&route.ed25519).ok_or_else(|| {
                eyre::eyre!(
                    "routing identity {} has no finalized authority",
                    route.ed25519
                )
            })?;
            eyre::ensure!(
                authority.domain.zone_id == route.zone_id,
                "routing Zone does not match installed authority domain"
            );
            let roster = self.rosters.get(&route.zone_id).ok_or_else(|| {
                eyre::eyre!("Zone {} has no installed finalized roster", route.zone_id)
            })?;
            eyre::ensure!(
                roster.domain == authority.domain
                    && roster.members.contains(&authority.certificate_member),
                "peer authority does not belong to its exact finalized roster"
            );
        }
        for (zone_id, roster) in &self.rosters {
            let members = self
                .peers
                .values()
                .filter(|peer| peer.domain.zone_id == *zone_id)
                .map(|peer| peer.certificate_member)
                .collect::<BTreeSet<_>>();
            eyre::ensure!(
                members == roster.members.into_iter().collect(),
                "installed peer bindings do not exactly cover finalized roster {zone_id}"
            );
        }
        let local = self.peers.get(local_ed25519).ok_or_else(|| {
            eyre::eyre!("local Commonware identity has no installed ECDSA authority")
        })?;
        eyre::ensure!(
            local.domain.zone_id == routing.local_zone_id
                && local.certificate_member == local_ecdsa,
            "local inter-Zone identity does not match the finalized local roster member"
        );
        Ok(())
    }
}

/// One private outbound request. `target` is an exact configured Ed25519 identity, not an endpoint.
#[derive(Debug)]
pub struct InterZoneServiceRequest {
    pub target: PublicKey,
    pub remote_zone_id: u32,
    pub stream: u64,
    pub sequence: u64,
    pub payload: Vec<u8>,
    pub response: oneshot::Sender<Result<InterZoneAcknowledgment, String>>,
}

/// Durable acknowledgment returned over the same authenticated peer identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterZoneAcknowledgment {
    pub authenticated_peer: PublicKey,
    pub remote_member: Address,
    pub remote_domain: ZoneDomain,
    pub stream: u64,
    pub sequence: u64,
}

/// Verified inbound delivery. The response must be completed only after durable service ingestion;
/// dropping it intentionally withholds the network acknowledgment.
#[derive(Debug)]
pub struct AuthenticatedInterZoneRequest {
    pub authenticated_peer: PublicKey,
    pub session: AuthenticatedPeerSession,
    pub stream: u64,
    pub sequence: u64,
    pub payload: Vec<u8>,
    pub response: oneshot::Sender<Result<(), String>>,
}

/// Bounded runtime ports exported independently from intra-Zone Raft ports.
pub struct InterZoneServicePorts {
    pub authority: mpsc::Sender<InterZoneAuthoritySet>,
    pub requests: mpsc::Sender<InterZoneServiceRequest>,
    pub incoming: mpsc::Receiver<AuthenticatedInterZoneRequest>,
}

pub(crate) struct InterZoneNodeChannels {
    pub authority: mpsc::Receiver<InterZoneAuthoritySet>,
    pub requests: mpsc::Receiver<InterZoneServiceRequest>,
    pub incoming: mpsc::Sender<AuthenticatedInterZoneRequest>,
}

pub(crate) fn channels() -> (InterZoneServicePorts, InterZoneNodeChannels) {
    let (authority_tx, authority) = mpsc::channel(1);
    let (requests_tx, requests) = mpsc::channel(PORT_BACKLOG);
    let (incoming, incoming_rx) = mpsc::channel(PORT_BACKLOG);
    (
        InterZoneServicePorts {
            authority: authority_tx,
            requests: requests_tx,
            incoming: incoming_rx,
        },
        InterZoneNodeChannels {
            authority,
            requests,
            incoming,
        },
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Transcript {
    request_id: B256,
    initiator_ed25519: PublicKey,
    responder_ed25519: PublicKey,
    initiator_domain: ZoneDomain,
    initiator_member: Address,
    responder_domain: ZoneDomain,
    responder_member: Address,
    initiator_nonce: B256,
    responder_nonce: B256,
    stream: u64,
    sequence: u64,
}

impl Transcript {
    fn proof(
        &self,
        signer: &PrivateKeySigner,
        responder_role: bool,
    ) -> eyre::Result<TransportSessionProof> {
        let mut proof = TransportSessionProof {
            request_id: self.request_id,
            initiator_ed25519: B256::from_slice(self.initiator_ed25519.as_ref()),
            responder_ed25519: B256::from_slice(self.responder_ed25519.as_ref()),
            initiator: self.initiator_domain,
            initiator_member: self.initiator_member,
            responder: self.responder_domain,
            responder_member: self.responder_member,
            initiator_nonce: self.initiator_nonce,
            responder_nonce: self.responder_nonce,
            stream: self.stream,
            sequence: self.sequence,
            responder_role,
            signature: SignatureBytes([0; 65]),
        };
        let signature = signer
            .sign_hash_sync(&proof.session_hash())
            .map_err(|error| eyre::eyre!("failed signing inter-Zone session proof: {error}"))?;
        proof.signature = SignatureBytes(signature.as_bytes());
        Ok(proof)
    }

    fn validate_proof_fields(&self, proof: &TransportSessionProof, role: bool) -> bool {
        proof.request_id == self.request_id
            && proof.initiator_ed25519 == B256::from_slice(self.initiator_ed25519.as_ref())
            && proof.responder_ed25519 == B256::from_slice(self.responder_ed25519.as_ref())
            && proof.stream == self.stream
            && proof.sequence == self.sequence
            && proof.initiator == self.initiator_domain
            && proof.initiator_member == self.initiator_member
            && proof.responder == self.responder_domain
            && proof.responder_member == self.responder_member
            && proof.initiator_nonce == self.initiator_nonce
            && proof.responder_nonce == self.responder_nonce
            && proof.responder_role == role
    }
}

struct OutboundPending {
    transcript: Transcript,
    payload: Vec<u8>,
    response: oneshot::Sender<Result<InterZoneAcknowledgment, String>>,
    expires: Instant,
}

struct InboundChallenge {
    transcript: Transcript,
    responder_proof: TransportSessionProof,
    expires: Instant,
}

pub(crate) async fn run(
    context: commonware_runtime::tokio::Context,
    routing: InterZoneRoutingConfig,
    ed25519_private_key: PrivateKey,
    ecdsa_signer: PrivateKeySigner,
    channels: InterZoneNodeChannels,
) -> eyre::Result<()> {
    let local_ed25519 = ed25519_private_key.public_key();
    let max_peers = NonZeroUsize::new(routing.peers.len()).expect("validated nonempty routing");
    let namespace = inter_zone_namespace(routing.l1_chain_id);
    let config = network::setup_commonware_config(
        ed25519_private_key,
        &namespace,
        routing.listen,
        max_peers,
        routing.bypass_ip_check,
        MAX_INTER_ZONE_MESSAGE_SIZE,
    );
    let primary = Map::try_from(
        routing
            .peers
            .iter()
            .map(|peer| (peer.ed25519.clone(), peer.endpoint.to_commonware()))
            .collect::<Vec<_>>(),
    )?;
    let secondary = Map::try_from(Vec::<(PublicKey, commonware_p2p::Address)>::new())?;
    let peers = AddressableTrackedPeers::new(primary, secondary);
    let (mut network, mut oracle) = lookup::Network::new(context.child("network"), config);
    oracle.track(0, peers);
    let (request_sender, request_receiver) =
        network.register(INTER_ZONE_REQUEST_CHANNEL, service_quota());
    let (response_sender, response_receiver) =
        network.register(INTER_ZONE_RESPONSE_CHANNEL, service_quota());
    let mut network_task = network.start();
    let service = run_service(
        local_ed25519,
        ecdsa_signer,
        routing,
        channels.authority,
        channels.requests,
        channels.incoming,
        request_sender,
        request_receiver,
        response_sender,
        response_receiver,
    );
    tokio::pin!(service);
    tokio::select! {
        result = &mut service => result,
        result = &mut network_task => match result {
            Ok(()) => Err(eyre::eyre!("inter-Zone Commonware network stopped unexpectedly")),
            Err(error) => Err(eyre::eyre!("inter-Zone Commonware network failed: {error}")),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_service(
    local_ed25519: PublicKey,
    ecdsa_signer: PrivateKeySigner,
    routing: InterZoneRoutingConfig,
    mut authority_updates: mpsc::Receiver<InterZoneAuthoritySet>,
    mut outbound_requests: mpsc::Receiver<InterZoneServiceRequest>,
    incoming: mpsc::Sender<AuthenticatedInterZoneRequest>,
    mut request_sender: CommonwareSender,
    mut request_receiver: CommonwareReceiver,
    response_sender: CommonwareSender,
    mut response_receiver: CommonwareReceiver,
) -> eyre::Result<()> {
    let mut authority: Option<InterZoneAuthoritySet> = None;
    let mut outbound = HashMap::<B256, OutboundPending>::new();
    let mut challenges = HashMap::<B256, InboundChallenge>::new();
    let mut used_initiator_nonces = HashMap::<(PublicKey, B256), Instant>::new();
    let mut cleanup = tokio::time::interval(CLEANUP_INTERVAL);
    loop {
        tokio::select! {
            update = authority_updates.recv() => {
                let update = update.ok_or_else(|| eyre::eyre!("inter-Zone authority port closed"))?;
                update.validate(&routing, &local_ed25519, ecdsa_signer.address())?;
                match &authority {
                    Some(installed) if installed != &update => {
                        return Err(eyre::eyre!("conflicting inter-Zone authority replacement"));
                    }
                    Some(_) => {}
                    None => authority = Some(update),
                }
            }
            request = outbound_requests.recv() => {
                let request = request.ok_or_else(|| eyre::eyre!("inter-Zone request port closed"))?;
                let result = begin_outbound(
                    authority.as_ref(),
                    &routing,
                    &local_ed25519,
                    request,
                    &mut request_sender,
                    &mut outbound,
                );
                if let Err((response, error)) = result {
                    let _ = response.send(Err(error));
                }
            }
            received = request_receiver.recv() => {
                let (peer, bytes) = received.map_err(|error| eyre::eyre!("inter-Zone request receiver failed: {error}"))?;
                handle_request(
                    authority.as_ref(),
                    &local_ed25519,
                    &ecdsa_signer,
                    peer,
                    bytes,
                    &incoming,
                    &response_sender,
                    &mut challenges,
                    &mut used_initiator_nonces,
                ).await;
            }
            received = response_receiver.recv() => {
                let (peer, bytes) = received.map_err(|error| eyre::eyre!("inter-Zone response receiver failed: {error}"))?;
                handle_response(
                    authority.as_ref(),
                    &local_ed25519,
                    &ecdsa_signer,
                    peer,
                    bytes,
                    &mut request_sender,
                    &mut outbound,
                );
            }
            _ = cleanup.tick() => {
                let now = Instant::now();
                challenges.retain(|_, challenge| challenge.expires > now);
                used_initiator_nonces.retain(|_, expires| *expires > now);
                let expired = outbound
                    .iter()
                    .filter_map(|(request_id, pending)| {
                        (pending.expires <= now).then_some(*request_id)
                    })
                    .collect::<Vec<_>>();
                for request_id in expired {
                    if let Some(pending) = outbound.remove(&request_id) {
                        let _ = pending.response.send(Err("inter-Zone session handshake expired".to_owned()));
                    }
                }
            }
        }
    }
}

fn begin_outbound(
    authority: Option<&InterZoneAuthoritySet>,
    routing: &InterZoneRoutingConfig,
    local_ed25519: &PublicKey,
    request: InterZoneServiceRequest,
    sender: &mut CommonwareSender,
    pending: &mut HashMap<B256, OutboundPending>,
) -> Result<
    (),
    (
        oneshot::Sender<Result<InterZoneAcknowledgment, String>>,
        String,
    ),
> {
    let mut response = Some(request.response);
    let result = (|| {
        let authority = authority.ok_or("finalized inter-Zone authority is not installed")?;
        if request.payload.len() > MAX_SERVICE_ENVELOPE_BYTES {
            return Err("inter-Zone service payload exceeds its canonical bound");
        }
        let local = authority
            .peers
            .get(local_ed25519)
            .ok_or("local inter-Zone authority is missing")?;
        let remote = authority
            .peers
            .get(&request.target)
            .ok_or("target Ed25519 identity has no finalized authority")?;
        if remote.domain.zone_id != request.remote_zone_id
            || remote.domain.zone_id == routing.local_zone_id
            || request.stream == 0
        {
            return Err("inter-Zone request target/domain/stream mismatch");
        }
        let mut random = [0u8; 64];
        OsRng.fill_bytes(&mut random);
        let request_id = B256::from_slice(&random[..32]);
        let initiator_nonce = B256::from_slice(&random[32..]);
        if request_id.is_zero() || initiator_nonce.is_zero() || pending.contains_key(&request_id) {
            return Err("OS CSPRNG produced an unusable inter-Zone session identifier");
        }
        let transcript = Transcript {
            request_id,
            initiator_ed25519: local_ed25519.clone(),
            responder_ed25519: request.target.clone(),
            initiator_domain: local.domain,
            initiator_member: local.certificate_member,
            responder_domain: remote.domain,
            responder_member: remote.certificate_member,
            initiator_nonce,
            responder_nonce: B256::ZERO,
            stream: request.stream,
            sequence: request.sequence,
        };
        let frame = encode_challenge(&transcript);
        let admitted = sender.send(Recipients::One(request.target), frame, true);
        if admitted.len() != 1 {
            return Err("inter-Zone challenge was not admitted by Commonware");
        }
        pending.insert(
            request_id,
            OutboundPending {
                transcript,
                payload: request.payload,
                response: response.take().expect("response is inserted once"),
                expires: Instant::now() + SESSION_TIMEOUT,
            },
        );
        Ok(())
    })();
    result.map_err(|error| {
        (
            response.expect("failed request retains its response"),
            error.to_owned(),
        )
    })
}

async fn handle_request(
    authority: Option<&InterZoneAuthoritySet>,
    local_ed25519: &PublicKey,
    signer: &PrivateKeySigner,
    peer: PublicKey,
    bytes: IoBuf,
    incoming: &mpsc::Sender<AuthenticatedInterZoneRequest>,
    response_sender: &CommonwareSender,
    challenges: &mut HashMap<B256, InboundChallenge>,
    used_initiator_nonces: &mut HashMap<(PublicKey, B256), Instant>,
) {
    let bytes: Vec<u8> = bytes.into();
    let Some(authority) = authority else {
        return;
    };
    let Some(tag) = bytes.get(1).copied() else {
        return;
    };
    match tag {
        CHALLENGE_TAG => {
            let Ok(mut transcript) = decode_challenge(&bytes) else {
                return;
            };
            if transcript.initiator_ed25519 != peer
                || transcript.responder_ed25519 != *local_ed25519
                || transcript.initiator_nonce.is_zero()
                || transcript.stream == 0
                || !authority_matches(authority, &transcript)
                || challenges.contains_key(&transcript.request_id)
                || used_initiator_nonces.contains_key(&(peer.clone(), transcript.initiator_nonce))
            {
                return;
            }
            let mut nonce = [0u8; 32];
            OsRng.fill_bytes(&mut nonce);
            transcript.responder_nonce = B256::from(nonce);
            if transcript.responder_nonce.is_zero() {
                return;
            }
            let Ok(proof) = transcript.proof(signer, true) else {
                return;
            };
            let initiator_nonce = transcript.initiator_nonce;
            let frame = encode_challenge_response(&transcript, &proof);
            send_response(response_sender, peer.clone(), frame);
            challenges.insert(
                transcript.request_id,
                InboundChallenge {
                    transcript,
                    responder_proof: proof,
                    expires: Instant::now() + SESSION_TIMEOUT,
                },
            );
            used_initiator_nonces.insert(
                (peer, initiator_nonce),
                Instant::now() + SESSION_REPLAY_WINDOW,
            );
        }
        DELIVERY_TAG => {
            let Ok((transcript, initiator_proof, responder_proof, payload)) =
                decode_delivery(&bytes)
            else {
                return;
            };
            let Some(challenge) = challenges.remove(&transcript.request_id) else {
                return;
            };
            if challenge.expires <= Instant::now()
                || challenge.transcript != transcript
                || challenge.responder_proof != responder_proof
                || transcript.initiator_ed25519 != peer
                || !transcript.validate_proof_fields(&initiator_proof, false)
                || !transcript.validate_proof_fields(&responder_proof, true)
                || !authority_matches(authority, &transcript)
            {
                return;
            }
            let Some(local_roster) = authority.rosters.get(&transcript.responder_domain.zone_id)
            else {
                return;
            };
            let Some(remote_roster) = authority.rosters.get(&transcript.initiator_domain.zone_id)
            else {
                return;
            };
            let Ok(session) = AuthenticatedPeerSession::establish(
                local_roster.clone(),
                remote_roster.clone(),
                &responder_proof,
                &initiator_proof,
            ) else {
                return;
            };
            let (ack, ack_rx) = oneshot::channel();
            let request = AuthenticatedInterZoneRequest {
                authenticated_peer: peer.clone(),
                session,
                stream: transcript.stream,
                sequence: transcript.sequence,
                payload,
                response: ack,
            };
            if incoming.send(request).await.is_err() {
                return;
            }
            let sender = response_sender.clone();
            contextless_spawn(async move {
                match ack_rx.await {
                    Ok(Ok(())) => send_response(
                        &sender,
                        peer,
                        encode_ack(&transcript, transcript.responder_member),
                    ),
                    Ok(Err(error)) => {
                        send_response(&sender, peer, encode_error(transcript.request_id, &error))
                    }
                    Err(_) => {}
                }
            });
        }
        _ => {}
    }
}

fn handle_response(
    authority: Option<&InterZoneAuthoritySet>,
    local_ed25519: &PublicKey,
    signer: &PrivateKeySigner,
    peer: PublicKey,
    bytes: IoBuf,
    request_sender: &mut CommonwareSender,
    outbound: &mut HashMap<B256, OutboundPending>,
) {
    let bytes: Vec<u8> = bytes.into();
    let Some(tag) = bytes.get(1).copied() else {
        return;
    };
    match tag {
        CHALLENGE_RESPONSE_TAG => {
            let Ok((transcript, responder_proof)) = decode_challenge_response(&bytes) else {
                return;
            };
            let Some(pending) = outbound.get_mut(&transcript.request_id) else {
                return;
            };
            if pending.expires <= Instant::now()
                || pending.transcript.responder_ed25519 != peer
                || transcript_without_responder_nonce(&pending.transcript)
                    != transcript_without_responder_nonce(&transcript)
                || transcript.responder_nonce.is_zero()
                || !transcript.validate_proof_fields(&responder_proof, true)
                || authority.is_none_or(|authority| !authority_matches(authority, &transcript))
            {
                return;
            }
            let Ok(initiator_proof) = transcript.proof(signer, false) else {
                return;
            };
            let Some(authority) = authority else {
                return;
            };
            let (Some(local_roster), Some(remote_roster)) = (
                authority.rosters.get(&transcript.initiator_domain.zone_id),
                authority.rosters.get(&transcript.responder_domain.zone_id),
            ) else {
                return;
            };
            if AuthenticatedPeerSession::establish(
                local_roster.clone(),
                remote_roster.clone(),
                &initiator_proof,
                &responder_proof,
            )
            .is_err()
            {
                return;
            }
            pending.transcript = transcript.clone();
            let frame = encode_delivery(
                &transcript,
                &initiator_proof,
                &responder_proof,
                &pending.payload,
            );
            if request_sender
                .send(Recipients::One(peer), frame, true)
                .len()
                != 1
                && let Some(pending) = outbound.remove(&transcript.request_id)
            {
                let _ = pending.response.send(Err(
                    "inter-Zone delivery was not admitted by Commonware".to_owned(),
                ));
            }
        }
        ACK_TAG => {
            let Ok((request_id, stream, sequence, member)) = decode_ack(&bytes) else {
                return;
            };
            let Some(pending) = outbound.remove(&request_id) else {
                return;
            };
            let transcript = pending.transcript;
            if peer != transcript.responder_ed25519
                || stream != transcript.stream
                || sequence != transcript.sequence
                || member != transcript.responder_member
                || local_ed25519 != &transcript.initiator_ed25519
            {
                let _ = pending.response.send(Err(
                    "inter-Zone acknowledgment identity or sequence mismatch".to_owned(),
                ));
                return;
            }
            let _ = pending.response.send(Ok(InterZoneAcknowledgment {
                authenticated_peer: peer,
                remote_member: member,
                remote_domain: transcript.responder_domain,
                stream,
                sequence,
            }));
        }
        ERROR_TAG => {
            let Ok((request_id, error)) = decode_error(&bytes) else {
                return;
            };
            if let Some(pending) = outbound.remove(&request_id)
                && peer == pending.transcript.responder_ed25519
            {
                let _ = pending.response.send(Err(error));
            }
        }
        _ => {}
    }
}

fn authority_matches(authority: &InterZoneAuthoritySet, transcript: &Transcript) -> bool {
    authority
        .peers
        .get(&transcript.initiator_ed25519)
        .is_some_and(|peer| {
            peer.domain == transcript.initiator_domain
                && peer.certificate_member == transcript.initiator_member
        })
        && authority
            .peers
            .get(&transcript.responder_ed25519)
            .is_some_and(|peer| {
                peer.domain == transcript.responder_domain
                    && peer.certificate_member == transcript.responder_member
            })
        && transcript.initiator_domain.zone_id != transcript.responder_domain.zone_id
}

fn transcript_without_responder_nonce(transcript: &Transcript) -> Vec<u8> {
    let mut transcript = transcript.clone();
    transcript.responder_nonce = B256::ZERO;
    encode_transcript(&transcript)
}

fn send_response(sender: &CommonwareSender, peer: PublicKey, frame: Vec<u8>) {
    let mut sender = sender.clone();
    if sender
        .send(Recipients::One(peer.clone()), frame, true)
        .len()
        != 1
    {
        debug!(target: "zone::p2p", %peer, "inter-Zone response was not admitted");
    }
}

fn contextless_spawn(future: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(future);
}

fn service_quota() -> Quota {
    Quota::per_second(NZU32!(128)).allow_burst(NZU32!(32))
}

fn inter_zone_namespace(l1_chain_id: u64) -> Vec<u8> {
    let mut namespace = Vec::with_capacity(INTER_ZONE_NAMESPACE.len() + 8);
    namespace.extend_from_slice(INTER_ZONE_NAMESPACE);
    namespace.extend_from_slice(&l1_chain_id.to_be_bytes());
    namespace
}

fn encode_challenge(transcript: &Transcript) -> Vec<u8> {
    let mut out = vec![WIRE_VERSION, CHALLENGE_TAG];
    encode_transcript_to(transcript, &mut out);
    out
}

fn decode_challenge(bytes: &[u8]) -> Result<Transcript, ()> {
    let mut reader = WireReader::new(bytes, CHALLENGE_TAG)?;
    let transcript = reader.transcript()?;
    reader.finish()?;
    Ok(transcript)
}

fn encode_challenge_response(transcript: &Transcript, proof: &TransportSessionProof) -> Vec<u8> {
    let mut out = vec![WIRE_VERSION, CHALLENGE_RESPONSE_TAG];
    encode_transcript_to(transcript, &mut out);
    put_bytes(&mut out, &proof.canonical_bytes());
    out
}

fn decode_challenge_response(bytes: &[u8]) -> Result<(Transcript, TransportSessionProof), ()> {
    let mut reader = WireReader::new(bytes, CHALLENGE_RESPONSE_TAG)?;
    let transcript = reader.transcript()?;
    let proof = TransportSessionProof::decode(reader.bytes(4096)?).map_err(|_| ())?;
    reader.finish()?;
    Ok((transcript, proof))
}

fn encode_delivery(
    transcript: &Transcript,
    initiator: &TransportSessionProof,
    responder: &TransportSessionProof,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = vec![WIRE_VERSION, DELIVERY_TAG];
    encode_transcript_to(transcript, &mut out);
    put_bytes(&mut out, &initiator.canonical_bytes());
    put_bytes(&mut out, &responder.canonical_bytes());
    put_bytes(&mut out, payload);
    out
}

fn decode_delivery(
    bytes: &[u8],
) -> Result<
    (
        Transcript,
        TransportSessionProof,
        TransportSessionProof,
        Vec<u8>,
    ),
    (),
> {
    let mut reader = WireReader::new(bytes, DELIVERY_TAG)?;
    let transcript = reader.transcript()?;
    let initiator = TransportSessionProof::decode(reader.bytes(4096)?).map_err(|_| ())?;
    let responder = TransportSessionProof::decode(reader.bytes(4096)?).map_err(|_| ())?;
    let payload = reader.bytes(MAX_SERVICE_ENVELOPE_BYTES)?.to_vec();
    reader.finish()?;
    Ok((transcript, initiator, responder, payload))
}

fn encode_ack(transcript: &Transcript, member: Address) -> Vec<u8> {
    let mut out = vec![WIRE_VERSION, ACK_TAG];
    out.extend_from_slice(transcript.request_id.as_slice());
    out.extend_from_slice(&transcript.stream.to_be_bytes());
    out.extend_from_slice(&transcript.sequence.to_be_bytes());
    out.extend_from_slice(member.as_slice());
    out
}

fn decode_ack(bytes: &[u8]) -> Result<(B256, u64, u64, Address), ()> {
    let mut reader = WireReader::new(bytes, ACK_TAG)?;
    let value = (
        reader.b256()?,
        reader.u64()?,
        reader.u64()?,
        reader.address()?,
    );
    reader.finish()?;
    Ok(value)
}

fn encode_error(request_id: B256, error: &str) -> Vec<u8> {
    let mut out = vec![WIRE_VERSION, ERROR_TAG];
    out.extend_from_slice(request_id.as_slice());
    let sanitized = keccak256(error.as_bytes());
    out.extend_from_slice(sanitized.as_slice());
    out
}

fn decode_error(bytes: &[u8]) -> Result<(B256, String), ()> {
    let mut reader = WireReader::new(bytes, ERROR_TAG)?;
    let request_id = reader.b256()?;
    let code = reader.b256()?;
    reader.finish()?;
    Ok((
        request_id,
        format!("remote durable ingestion rejected ({code})"),
    ))
}

fn encode_transcript(transcript: &Transcript) -> Vec<u8> {
    let mut out = Vec::new();
    encode_transcript_to(transcript, &mut out);
    out
}

fn encode_transcript_to(transcript: &Transcript, out: &mut Vec<u8>) {
    out.extend_from_slice(transcript.request_id.as_slice());
    out.extend_from_slice(transcript.initiator_ed25519.as_ref());
    out.extend_from_slice(transcript.responder_ed25519.as_ref());
    transcript.initiator_domain.encode_to(out);
    out.extend_from_slice(transcript.initiator_member.as_slice());
    transcript.responder_domain.encode_to(out);
    out.extend_from_slice(transcript.responder_member.as_slice());
    out.extend_from_slice(transcript.initiator_nonce.as_slice());
    out.extend_from_slice(transcript.responder_nonce.as_slice());
    out.extend_from_slice(&transcript.stream.to_be_bytes());
    out.extend_from_slice(&transcript.sequence.to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let length = u32::try_from(bytes.len()).expect("bounded inter-Zone field fits u32");
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(bytes);
}

struct WireReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> WireReader<'a> {
    fn new(bytes: &'a [u8], expected_tag: u8) -> Result<Self, ()> {
        if bytes.len() > MAX_INTER_ZONE_MESSAGE_SIZE as usize
            || bytes.first() != Some(&WIRE_VERSION)
            || bytes.get(1) != Some(&expected_tag)
        {
            return Err(());
        }
        Ok(Self { bytes, offset: 2 })
    }

    fn transcript(&mut self) -> Result<Transcript, ()> {
        let request_id = self.b256()?;
        let initiator_ed25519 = decode_public_key(self.take(32)?)?;
        let responder_ed25519 = decode_public_key(self.take(32)?)?;
        let initiator_domain = self.domain()?;
        let initiator_member = self.address()?;
        let responder_domain = self.domain()?;
        let responder_member = self.address()?;
        let initiator_nonce = self.b256()?;
        let responder_nonce = self.b256()?;
        let stream = self.u64()?;
        let sequence = self.u64()?;
        Ok(Transcript {
            request_id,
            initiator_ed25519,
            responder_ed25519,
            initiator_domain,
            initiator_member,
            responder_domain,
            responder_member,
            initiator_nonce,
            responder_nonce,
            stream,
            sequence,
        })
    }

    fn domain(&mut self) -> Result<ZoneDomain, ()> {
        Ok(ZoneDomain {
            l1_chain_id: self.u64()?,
            zone_id: self.u32()?,
            chain_id: self.u64()?,
            portal: self.address()?,
            authority_epoch: self.u64()?,
            roster_hash: self.b256()?,
            protocol_version: self.u16()?,
        })
    }

    fn bytes(&mut self, maximum: usize) -> Result<&'a [u8], ()> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(());
        }
        self.take(length)
    }

    fn address(&mut self) -> Result<Address, ()> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn b256(&mut self) -> Result<B256, ()> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn u32(&mut self) -> Result<u32, ()> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| ())?,
        ))
    }

    fn u16(&mut self) -> Result<u16, ()> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| ())?,
        ))
    }

    fn u64(&mut self) -> Result<u64, ()> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| ())?,
        ))
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ()> {
        let end = self.offset.checked_add(length).ok_or(())?;
        let value = self.bytes.get(self.offset..end).ok_or(())?;
        self.offset = end;
        Ok(value)
    }

    fn finish(&self) -> Result<(), ()> {
        (self.offset == self.bytes.len()).then_some(()).ok_or(())
    }
}

fn decode_public_key(bytes: &[u8]) -> Result<PublicKey, ()> {
    use commonware_codec::DecodeExt as _;
    PublicKey::decode(bytes).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Signer as _;

    fn domain(zone_id: u32) -> ZoneDomain {
        ZoneDomain {
            l1_chain_id: 1,
            zone_id,
            chain_id: 1_000 + u64::from(zone_id),
            portal: Address::with_last_byte(zone_id as u8 + 1),
            authority_epoch: 7,
            roster_hash: B256::with_last_byte(zone_id as u8 + 1),
            protocol_version: 1,
        }
    }

    fn transcript() -> Transcript {
        Transcript {
            request_id: B256::repeat_byte(1),
            initiator_ed25519: PrivateKey::from_seed(1).public_key(),
            responder_ed25519: PrivateKey::from_seed(2).public_key(),
            initiator_domain: domain(1),
            initiator_member: Address::repeat_byte(3),
            responder_domain: domain(2),
            responder_member: Address::repeat_byte(4),
            initiator_nonce: B256::repeat_byte(5),
            responder_nonce: B256::repeat_byte(6),
            stream: 7,
            sequence: 8,
        }
    }

    fn proof(transcript: &Transcript, responder_role: bool) -> TransportSessionProof {
        TransportSessionProof {
            request_id: transcript.request_id,
            initiator_ed25519: B256::from_slice(transcript.initiator_ed25519.as_ref()),
            responder_ed25519: B256::from_slice(transcript.responder_ed25519.as_ref()),
            initiator: transcript.initiator_domain,
            initiator_member: transcript.initiator_member,
            responder: transcript.responder_domain,
            responder_member: transcript.responder_member,
            initiator_nonce: transcript.initiator_nonce,
            responder_nonce: transcript.responder_nonce,
            stream: transcript.stream,
            sequence: transcript.sequence,
            responder_role,
            signature: SignatureBytes([9; 65]),
        }
    }

    #[test]
    fn handshake_wire_is_exact_and_bounded() {
        let transcript = transcript();
        let initiator = proof(&transcript, false);
        let responder = proof(&transcript, true);
        let payload = vec![0xabu8; MAX_SERVICE_ENVELOPE_BYTES];
        let encoded = encode_delivery(&transcript, &initiator, &responder, &payload);
        assert!(encoded.len() <= MAX_INTER_ZONE_MESSAGE_SIZE as usize);
        let decoded = decode_delivery(&encoded).unwrap();
        assert_eq!(decoded.0, transcript);
        assert_eq!(decoded.1, initiator);
        assert_eq!(decoded.2, responder);
        assert_eq!(decoded.3, payload);

        let mut trailing = encoded;
        trailing.push(0);
        assert!(decode_delivery(&trailing).is_err());
    }

    #[test]
    fn transcript_mutations_change_wire_identity() {
        let original = transcript();
        for changed in [
            Transcript {
                stream: original.stream + 1,
                ..original.clone()
            },
            Transcript {
                sequence: original.sequence + 1,
                ..original.clone()
            },
            Transcript {
                initiator_ed25519: PrivateKey::from_seed(9).public_key(),
                ..original.clone()
            },
            Transcript {
                responder_ed25519: PrivateKey::from_seed(10).public_key(),
                ..original.clone()
            },
        ] {
            assert_ne!(encode_transcript(&original), encode_transcript(&changed));
        }
    }

    #[test]
    fn signed_proof_binds_channel_domains_and_delivery_coordinate() {
        let transcript = transcript();
        let original = proof(&transcript, false);
        let original_hash = original.session_hash();
        let mut mutations = Vec::new();

        let mut changed = original.clone();
        changed.request_id = B256::repeat_byte(0x11);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.initiator_ed25519 = B256::repeat_byte(0x12);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.responder_ed25519 = B256::repeat_byte(0x13);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.initiator = domain(3);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.initiator_member = Address::repeat_byte(0x14);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.responder = domain(4);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.responder_member = Address::repeat_byte(0x15);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.initiator_nonce = B256::repeat_byte(0x16);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.responder_nonce = B256::repeat_byte(0x17);
        mutations.push(changed);
        let mut changed = original.clone();
        changed.stream += 1;
        mutations.push(changed);
        let mut changed = original.clone();
        changed.sequence += 1;
        mutations.push(changed);
        let mut changed = original;
        changed.responder_role = true;
        mutations.push(changed);

        for changed in mutations {
            assert_ne!(changed.session_hash(), original_hash);
        }
    }
}
