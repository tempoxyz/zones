//! At-least-once durable peer delivery contracts and monotonic records.

use std::time::Duration;

use alloy_primitives::B256;

/// Persistent delivery state for one logical message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryTransition {
    /// Payload is durable locally and awaits peer delivery.
    Queued,
    /// Peer durably acknowledged the frame. This is not a token outcome.
    TransportAcknowledged,
    /// A terminal destination certificate is durable, but source disposition remains.
    TerminalReceived,
    /// The matching source release/refund certificate is durable.
    SourceDisposed,
}

impl DeliveryTransition {
    const fn rank(self) -> u8 {
        match self {
            Self::Queued => 0,
            Self::TransportAcknowledged => 1,
            Self::TerminalReceived => 2,
            Self::SourceDisposed => 3,
        }
    }
}

/// Durable at-least-once record. Payload bytes are retained until protocol completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryRecord {
    /// Peer-specific stream incarnation.
    pub stream: u64,
    /// Contiguous sequence number in that stream.
    pub sequence: u64,
    /// Stable transfer ID represented by the payload.
    pub transfer_id: B256,
    /// Canonical bounded payload.
    pub payload: Vec<u8>,
    /// Monotonic protocol state.
    pub transition: DeliveryTransition,
    /// Number of send attempts, including the first.
    pub attempts: u32,
    /// Earliest monotonic-clock delay before another attempt.
    pub retry_after: Duration,
}

impl DeliveryRecord {
    /// Create a persisted-before-send record.
    pub fn queued(stream: u64, sequence: u64, transfer_id: B256, payload: Vec<u8>) -> Self {
        Self {
            stream,
            sequence,
            transfer_id,
            payload,
            transition: DeliveryTransition::Queued,
            attempts: 0,
            retry_after: Duration::ZERO,
        }
    }

    /// Advance monotonically. Skipping transport acknowledgment is permitted because a
    /// terminal certificate is stronger protocol evidence; regression is forbidden.
    pub fn advance(&mut self, next: DeliveryTransition) -> Result<bool, DeliveryError> {
        if next.rank() < self.transition.rank() {
            return Err(DeliveryError::Regression);
        }
        if next == self.transition {
            return Ok(false);
        }
        self.transition = next;
        Ok(true)
    }

    /// Record one retry using deterministic exponential backoff. The caller supplies
    /// persisted jitter so replay does not silently change the schedule.
    pub fn record_attempt(&mut self, jitter_millis: u64) -> Result<Duration, DeliveryError> {
        self.attempts = self
            .attempts
            .checked_add(1)
            .ok_or(DeliveryError::Overflow)?;
        let shift = self.attempts.saturating_sub(1).min(6);
        let base = 50u64.checked_shl(shift).unwrap_or(2_000).min(2_000);
        let jitter = jitter_millis.min(base / 4);
        self.retry_after = Duration::from_millis((base + jitter).min(2_000));
        Ok(self.retry_after)
    }

    /// Only a complete source disposition permits payload compaction.
    pub const fn compactable(&self) -> bool {
        matches!(self.transition, DeliveryTransition::SourceDisposed)
    }
}

/// Durable delivery failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DeliveryError {
    /// Durable state cannot move backward.
    #[error("delivery state regression")]
    Regression,
    /// Cursor would skip unresolved work or move backward.
    #[error("invalid delivery cursor advance")]
    InvalidCursor,
    /// Attempt accounting overflowed.
    #[error("delivery attempt counter overflow")]
    Overflow,
}

/// Storage integration required by the direct-stream worker.
///
/// Implementations must make each mutating call durable before returning success. In
/// particular, `persist_incoming` must finish before a transport acknowledgment is sent.
pub trait DeliveryStore: Send + Sync + 'static {
    /// Backend error.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Persist a complete outgoing frame before its first send.
    fn persist_outgoing(&self, peer_zone: u32, record: &DeliveryRecord) -> Result<(), Self::Error>;

    /// Persist an incoming frame before acknowledging durable receipt.
    fn persist_incoming(
        &self,
        peer_zone: u32,
        stream: u64,
        sequence: u64,
        payload: &[u8],
    ) -> Result<(), Self::Error>;

    /// Atomically persist a monotonic record transition.
    fn transition(
        &self,
        peer_zone: u32,
        stream: u64,
        sequence: u64,
        next: DeliveryTransition,
    ) -> Result<(), Self::Error>;

    /// Persist a contiguous cursor. Implementations must reject unresolved gaps.
    fn advance_contiguous_cursor(
        &self,
        peer_zone: u32,
        stream: u64,
        through_sequence: u64,
    ) -> Result<(), Self::Error>;

    /// Reconstruct all unfinished work after restart, ordered by stream and sequence.
    fn load_unfinished(&self, peer_zone: u32) -> Result<Vec<DeliveryRecord>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_is_monotonic_and_backoff_is_bounded() {
        let mut record = DeliveryRecord::queued(1, 1, B256::repeat_byte(1), vec![1, 2]);
        assert_eq!(
            record.advance(DeliveryTransition::TerminalReceived),
            Ok(true)
        );
        assert_eq!(
            record.advance(DeliveryTransition::TransportAcknowledged),
            Err(DeliveryError::Regression)
        );
        for _ in 0..20 {
            assert!(record.record_attempt(10_000).unwrap() <= Duration::from_secs(2));
        }
        assert!(!record.compactable());
        record.advance(DeliveryTransition::SourceDisposed).unwrap();
        assert!(record.compactable());
    }
}
