//! Typed node-facing ports and single-owner coordinator for the backfill protocol.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{Recipients, Sender as _, authenticated::lookup};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::{LeadershipSchedule, P2pPeerId, PeerTip};
use crate::{
    protocol::{RequestFrame, ResponseFrame},
    routing::{RoutingMembership, RoutingPolicy},
};

const BACKFILL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

type CommonwareSender = lookup::Sender<PublicKey, commonware_runtime::tokio::Context>;
type CommonwareReceiver = lookup::Receiver<PublicKey>;

/// Commands sent by node tasks to the backfill coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackfillCommand {
    /// Ask eligible peers for canonical blocks beginning at `start`.
    Request { start: u64 },
    /// Return one canonical block to the peer that requested it.
    SendBlock {
        peer: P2pPeerId,
        request_id: u64,
        block: Vec<u8>,
    },
    /// Finish one response page and advertise the responder's snapshot tip.
    Complete {
        peer: P2pPeerId,
        request_id: u64,
        tip: PeerTip,
    },
}

/// A process-lifetime request for canonical blocks from a node's provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillRequest {
    pub peer: P2pPeerId,
    pub request_id: u64,
    pub start: u64,
}

/// A response accepted for the active follower generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackfillResponse {
    Block { peer: P2pPeerId, block: Vec<u8> },
    Completed { peer: P2pPeerId, tip: PeerTip },
}

/// Bounded node-facing channels for backfill commands, serving requests, and responses.
pub struct BackfillPorts {
    pub commands: mpsc::Sender<BackfillCommand>,
    pub requests: mpsc::Receiver<BackfillRequest>,
    pub responses: mpsc::Receiver<BackfillResponse>,
}

/// Channels handed from the P2P runtime to the single backfill coordinator.
pub(crate) struct BackfillRuntimeChannels<Rq = CommonwareReceiver, Rs = CommonwareReceiver> {
    pub(crate) request_sender: CommonwareSender,
    pub(crate) request_receiver: Rq,
    pub(crate) response_sender: CommonwareSender,
    pub(crate) response_receiver: Rs,
    pub(crate) commands: mpsc::Receiver<BackfillCommand>,
    pub(crate) requests: mpsc::Sender<BackfillRequest>,
    pub(crate) responses: mpsc::Sender<BackfillResponse>,
}

#[derive(Debug, Clone, Copy)]
struct OutstandingBackfill {
    request_id: u64,
    sent_at: Instant,
}

impl OutstandingBackfill {
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

/// Owns every outstanding backfill request and the two Commonware backfill protocols.
pub(crate) struct BackfillCoordinator<Rq = CommonwareReceiver, Rs = CommonwareReceiver> {
    local: PublicKey,
    membership: RoutingMembership,
    leadership: LeadershipSchedule,
    request_sender: CommonwareSender,
    request_receiver: Rq,
    response_sender: CommonwareSender,
    response_receiver: Rs,
    commands: mpsc::Receiver<BackfillCommand>,
    requests: mpsc::Sender<BackfillRequest>,
    responses: mpsc::Sender<BackfillResponse>,
    job: BackfillJob,
}

impl<Rq, Rs> BackfillCoordinator<Rq, Rs>
where
    Rq: commonware_p2p::Receiver<PublicKey = PublicKey>,
    Rs: commonware_p2p::Receiver<PublicKey = PublicKey>,
{
    pub(crate) fn new(
        local: PublicKey,
        membership: RoutingMembership,
        leadership: LeadershipSchedule,
        channels: BackfillRuntimeChannels<Rq, Rs>,
    ) -> Self {
        Self {
            local,
            membership,
            leadership,
            request_sender: channels.request_sender,
            request_receiver: channels.request_receiver,
            response_sender: channels.response_sender,
            response_receiver: channels.response_receiver,
            commands: channels.commands,
            requests: channels.requests,
            responses: channels.responses,
            job: BackfillJob::default(),
        }
    }

    pub(crate) async fn run(mut self) -> eyre::Result<()> {
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let command = command.ok_or_else(|| eyre::eyre!("backfill command channel closed unexpectedly"))?;
                    self.handle_command(command).await?;
                }
                result = self.request_receiver.recv() => {
                    let (peer, bytes) = result.map_err(|err| eyre::eyre!("backfill request receive failed: {err}"))?;
                    self.handle_request(peer, bytes.as_ref()).await?;
                }
                result = self.response_receiver.recv() => {
                    let (peer, bytes) = result.map_err(|err| eyre::eyre!("backfill response receive failed: {err}"))?;
                    self.handle_response(peer, bytes.as_ref()).await?;
                }
            }
        }
    }

    async fn handle_command(&mut self, command: BackfillCommand) -> eyre::Result<()> {
        match command {
            BackfillCommand::Request { start } => self.request_blocks(start).await,
            BackfillCommand::SendBlock {
                peer,
                request_id,
                block,
            } => {
                let is_remote_member = self.leadership.with_authority(|authority| {
                    RoutingPolicy::new(&self.local, &self.membership, authority)
                        .is_remote_member(&peer)
                });
                if !is_remote_member {
                    warn!(target: "zone::p2p", %peer, "Ignoring backfill block addressed to an unknown peer");
                    return Ok(());
                }
                let frame = match (ResponseFrame::Block { request_id, block }).encode() {
                    Ok(frame) => frame,
                    Err(err) => {
                        error!(target: "zone::p2p", %peer, %err, "Backfill block exceeds the P2P response frame size limit");
                        return Ok(());
                    }
                };
                self.response_sender
                    .send(Recipients::Some(vec![peer]), frame, true)
                    .await
                    .map_err(|err| eyre::eyre!("failed sending backfill block: {err}"))?;
                Ok(())
            }
            BackfillCommand::Complete {
                peer,
                request_id,
                tip,
            } => {
                let is_remote_member = self.leadership.with_authority(|authority| {
                    RoutingPolicy::new(&self.local, &self.membership, authority)
                        .is_remote_member(&peer)
                });
                if !is_remote_member {
                    warn!(target: "zone::p2p", %peer, "Ignoring backfill completion addressed to an unknown peer");
                    return Ok(());
                }
                let frame = ResponseFrame::Complete { request_id, tip }
                    .encode()
                    .expect("completion frames have a fixed size");
                self.response_sender
                    .send(Recipients::Some(vec![peer]), frame, true)
                    .await
                    .map_err(|err| eyre::eyre!("failed completing block backfill: {err}"))?;
                Ok(())
            }
        }
    }

    async fn request_blocks(&mut self, start: u64) -> eyre::Result<()> {
        let (candidates, leader) = self.leadership.with_authority(|authority| {
            let policy = RoutingPolicy::new(&self.local, &self.membership, authority);
            (
                policy.backfill_candidates(),
                policy.preferred_backfill_leader(),
            )
        });
        let now = Instant::now();
        let leader_first = match &leader {
            Some(leader) => candidates.contains(leader) && !self.job.is_unresponsive(leader, now),
            None => false,
        };
        let mut attempts = Vec::with_capacity(2);
        if let Some(leader) = leader.filter(|_| leader_first) {
            attempts.push(vec![leader]);
        }
        attempts.push(candidates);

        for (attempt, sources) in attempts.into_iter().enumerate() {
            let leader_only = leader_first && attempt == 0;
            let request = self.job.begin_request(&sources, now);
            let Some((request_id, request_peers)) = request else {
                debug!(target: "zone::p2p", start, sources = sources.len(), leader_only, "Skipping block backfill request because all eligible peers already have outstanding responses");
                continue;
            };
            let request_frame = RequestFrame { request_id, start }.encode().to_vec();
            let sent = match self
                .request_sender
                .send(Recipients::Some(request_peers.clone()), request_frame, true)
                .await
            {
                Ok(sent) => sent,
                Err(err) => {
                    self.job.cancel_request(request_id);
                    return Err(eyre::eyre!("failed requesting block backfill: {err}"));
                }
            };
            self.job.finish_send(request_id, &sent);
            if sent.is_empty() {
                debug!(target: "zone::p2p", request_id, start, requested = request_peers.len(), leader_only, "Block backfill request reached no peer");
                continue;
            }
            if !leader_only {
                metrics::counter!("zone_p2p_backfill_requests_without_leader_total").increment(1);
            }
            debug!(target: "zone::p2p", request_id, start, connected = sent.len(), requested = request_peers.len(), sources = sources.len(), leader_only, "Sent block backfill request");
            break;
        }
        Ok(())
    }

    async fn handle_request(&mut self, peer: PublicKey, bytes: &[u8]) -> eyre::Result<()> {
        let request = match RequestFrame::decode(bytes) {
            Ok(request) => request,
            Err(err) => {
                warn!(target: "zone::p2p", %peer, size = bytes.len(), %err, "Ignoring malformed backfill request");
                return Ok(());
            }
        };
        let is_remote_member = self.leadership.with_authority(|authority| {
            RoutingPolicy::new(&self.local, &self.membership, authority).is_remote_member(&peer)
        });
        if !is_remote_member {
            warn!(target: "zone::p2p", %peer, "Ignoring backfill request from ineligible peer");
            return Ok(());
        }
        self.requests
            .send(BackfillRequest {
                peer,
                request_id: request.request_id,
                start: request.start,
            })
            .await
            .map_err(|_| eyre::eyre!("backfill request event channel closed"))
    }

    async fn handle_response(&mut self, peer: PublicKey, bytes: &[u8]) -> eyre::Result<()> {
        let frame = match ResponseFrame::decode(bytes) {
            Ok(frame) => frame,
            Err(err) => {
                warn!(target: "zone::p2p", %peer, size = bytes.len(), %err, "Ignoring malformed backfill response");
                return Ok(());
            }
        };
        let may_accept = self.leadership.with_authority(|authority| {
            RoutingPolicy::new(&self.local, &self.membership, authority)
                .may_accept_backfill_response(&peer)
        });
        if !may_accept {
            warn!(target: "zone::p2p", %peer, "Ignoring backfill response from ineligible peer");
            return Ok(());
        }
        let received_at = Instant::now();
        match frame {
            ResponseFrame::Block { request_id, block } => {
                if !self.job.accepts(&peer, request_id, received_at) {
                    warn!(target: "zone::p2p", %peer, request_id, "Ignoring unsolicited or stale backfill block");
                    return Ok(());
                }
                self.responses
                    .send(BackfillResponse::Block { peer, block })
                    .await
                    .map_err(|_| eyre::eyre!("backfill response event channel closed"))?;
            }
            ResponseFrame::Complete { request_id, tip } => {
                if !self.job.complete(&peer, request_id, received_at) {
                    warn!(target: "zone::p2p", %peer, request_id, "Ignoring unsolicited or stale backfill completion");
                    return Ok(());
                }
                self.responses
                    .send(BackfillResponse::Completed { peer, tip })
                    .await
                    .map_err(|_| eyre::eyre!("backfill response event channel closed"))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BACKFILL_RESPONSE_TIMEOUT, BackfillJob};
    use crate::protocol::{PeerTip, ResponseFrame};
    use alloy_primitives::B256;
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    fn peer(seed: u64) -> crate::P2pPeerId {
        PrivateKey::from_seed(seed).public_key()
    }

    fn tip() -> PeerTip {
        PeerTip {
            zone_height: 7,
            zone_hash: B256::repeat_byte(0x11),
            tempo_block_number: 13,
            tempo_block_hash: B256::repeat_byte(0x22),
        }
    }

    #[test]
    fn leader_request_expires_and_replacement_uses_wrapped_ids() {
        let leader = peer(1);
        let now = std::time::Instant::now();
        let mut job = BackfillJob::default();
        let (first, peers) = job
            .begin_request(std::slice::from_ref(&leader), now)
            .unwrap();
        job.finish_send(first, &peers);
        assert!(!job.is_unresponsive(&leader, now + BACKFILL_RESPONSE_TIMEOUT / 2));
        assert!(job.is_unresponsive(&leader, now + BACKFILL_RESPONSE_TIMEOUT));
        assert!(!job.complete(&leader, first, now + BACKFILL_RESPONSE_TIMEOUT));
        let (replacement, peers) = job
            .begin_request(
                std::slice::from_ref(&leader),
                now + BACKFILL_RESPONSE_TIMEOUT,
            )
            .unwrap();
        job.finish_send(replacement, &peers);
        assert_ne!(replacement, first);
        assert!(!job.accepts(&leader, first, now + BACKFILL_RESPONSE_TIMEOUT));
        assert!(!job.complete(&leader, first, now + BACKFILL_RESPONSE_TIMEOUT));
        assert!(job.accepts(&leader, replacement, now + BACKFILL_RESPONSE_TIMEOUT));
        assert!(job.complete(&leader, replacement, now + BACKFILL_RESPONSE_TIMEOUT));
    }

    #[test]
    fn malformed_completion_does_not_consume_outstanding_request() {
        let peer = peer(1);
        let now = std::time::Instant::now();
        let mut job = BackfillJob::default();
        let (request_id, peers) = job.begin_request(std::slice::from_ref(&peer), now).unwrap();
        job.finish_send(request_id, &peers);

        let mut malformed = vec![1];
        malformed.extend_from_slice(&request_id.to_be_bytes());
        malformed.extend_from_slice(&[0; PeerTip::ENCODED_LEN - 1]);
        assert!(ResponseFrame::decode(&malformed).is_err());
        assert!(job.accepts(&peer, request_id, now));
        assert!(job.complete(&peer, request_id, now));
        assert_eq!(
            ResponseFrame::decode(
                &ResponseFrame::Complete {
                    request_id,
                    tip: tip()
                }
                .encode()
                .unwrap()
            ),
            Ok(ResponseFrame::Complete {
                request_id,
                tip: tip()
            })
        );
    }
}
