//! Authenticated, bounded direct-peer session processing.
//!
//! Socket/TLS ownership stays in the node runtime. This module enforces the protocol boundary:
//! an encrypted channel is not usable until both peers prove possession of certificate-roster
//! keys over the same fresh transcript, and an incoming frame is persisted before an
//! acknowledgment can be returned.

use alloy_primitives::{Address, B256};
use zone_primitives::fast_transfer::{
    CanonicalEncode, MAX_PEER_MESSAGE_BYTES, PeerMessage, TransportSessionProof,
};

use crate::{DeliveryStore, EpochRoster, QuorumVerifier};

/// A mutually authenticated encrypted connection after transcript verification.
#[derive(Clone, Debug)]
pub struct AuthenticatedPeerSession {
    local: EpochRoster,
    remote: EpochRoster,
    local_member: Address,
    remote_member: Address,
    request_id: B256,
    stream: u64,
    sequence: u64,
}

impl AuthenticatedPeerSession {
    /// Authenticate both sides of one encrypted connection. Callers must generate fresh random
    /// nonces for every connection; all transcript fields must match except signature and role.
    pub fn establish(
        local: EpochRoster,
        remote: EpochRoster,
        local_proof: &TransportSessionProof,
        remote_proof: &TransportSessionProof,
    ) -> Result<Self, TransportError> {
        if local.domain.zone_id == remote.domain.zone_id
            || local.domain.l1_chain_id != remote.domain.l1_chain_id
            || local.domain.protocol_version != remote.domain.protocol_version
            || local_proof.initiator != remote_proof.initiator
            || local_proof.responder != remote_proof.responder
            || local_proof.initiator_member != remote_proof.initiator_member
            || local_proof.responder_member != remote_proof.responder_member
            || local_proof.initiator_nonce != remote_proof.initiator_nonce
            || local_proof.responder_nonce != remote_proof.responder_nonce
            || local_proof.request_id != remote_proof.request_id
            || local_proof.initiator_ed25519 != remote_proof.initiator_ed25519
            || local_proof.responder_ed25519 != remote_proof.responder_ed25519
            || local_proof.stream != remote_proof.stream
            || local_proof.sequence != remote_proof.sequence
            || local_proof.request_id.is_zero()
            || local_proof.initiator_nonce.is_zero()
            || local_proof.responder_nonce.is_zero()
            || local_proof.initiator_ed25519.is_zero()
            || local_proof.responder_ed25519.is_zero()
            || local_proof.initiator_ed25519 == local_proof.responder_ed25519
            || local_proof.responder_role == remote_proof.responder_role
        {
            return Err(TransportError::TranscriptMismatch);
        }
        let local_member = QuorumVerifier::new(local.clone())
            .verify_transport_session(local_proof)
            .map_err(|_| TransportError::Authentication)?;
        let remote_member = QuorumVerifier::new(remote.clone())
            .verify_transport_session(remote_proof)
            .map_err(|_| TransportError::Authentication)?;
        Ok(Self {
            local,
            remote,
            local_member,
            remote_member,
            request_id: local_proof.request_id,
            stream: local_proof.stream,
            sequence: local_proof.sequence,
        })
    }

    /// Remote certificate member authenticated for this connection.
    pub const fn remote_member(&self) -> Address {
        self.remote_member
    }

    /// Local certificate member authenticated for this connection.
    pub const fn local_member(&self) -> Address {
        self.local_member
    }

    /// Local authority domain.
    pub const fn local_roster(&self) -> &EpochRoster {
        &self.local
    }

    /// Remote authority domain.
    pub const fn remote_roster(&self) -> &EpochRoster {
        &self.remote
    }

    /// Fresh challenge identity authenticated by both enrolled members.
    pub const fn request_id(&self) -> B256 {
        self.request_id
    }

    /// Only this exact durable frame position was authenticated by this exchange.
    pub const fn authenticates_delivery(&self, stream: u64, sequence: u64) -> bool {
        self.stream == stream && self.sequence == sequence
    }

    /// Decode and durably store one frame before constructing its acknowledgment.
    pub fn persist_incoming<S: DeliveryStore>(
        &self,
        store: &S,
        stream: u64,
        sequence: u64,
        encoded: &[u8],
    ) -> Result<Vec<u8>, TransportError> {
        if !self.authenticates_delivery(stream, sequence) {
            return Err(TransportError::TranscriptMismatch);
        }
        let message = zone_primitives::fast_transfer::decode_exact::<PeerMessage>(
            encoded,
            MAX_PEER_MESSAGE_BYTES,
        )
        .map_err(|_| TransportError::MalformedFrame)?;
        if matches!(message, PeerMessage::Acknowledgment { .. }) {
            return Err(TransportError::UnexpectedAcknowledgment);
        }
        store
            .persist_incoming(self.remote.domain.zone_id, stream, sequence, encoded)
            .map_err(|error| TransportError::Storage(error.to_string()))?;
        Ok(PeerMessage::Acknowledgment { stream, sequence }.canonical_bytes())
    }
}

/// Direct-peer boundary failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TransportError {
    /// Both signed proofs did not describe one identical two-sided transcript.
    #[error("fast peer handshake transcript mismatch")]
    TranscriptMismatch,
    /// A proof was not signed by the claimed finalized roster member.
    #[error("fast peer authentication failed")]
    Authentication,
    /// Frame encoding or bounds were invalid.
    #[error("malformed fast peer frame")]
    MalformedFrame,
    /// An acknowledgment arrived on the data ingestion path.
    #[error("unexpected fast peer acknowledgment")]
    UnexpectedAcknowledgment,
    /// Durable receipt failed, so no acknowledgment may be sent.
    #[error("fast peer durable receipt failed: {0}")]
    Storage(String),
}
