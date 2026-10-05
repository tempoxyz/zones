//! Bounded admission and funded-pool accounting.

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, B256, U256};
use zone_primitives::fast_transfer::{AssetId, TransferIntent};

/// Deployment defaults from the protocol specification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolLimits {
    /// Incoming and outgoing unresolved transfers combined.
    pub unresolved_zone: usize,
    /// Unresolved transfers for one directed route.
    pub unresolved_route: usize,
    /// Unresolved transfers for one source sender.
    pub unresolved_sender: usize,
    /// All queued peer payload bytes.
    pub queued_zone_bytes: usize,
    /// One peer's queued payload bytes.
    pub queued_peer_bytes: usize,
    /// Active journal bytes before admission stops.
    pub journal_bytes: usize,
    /// Parallel certificate checks allowed for one peer.
    pub verification_concurrency_per_peer: usize,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            unresolved_zone: 10_000,
            unresolved_route: 1_000,
            unresolved_sender: 100,
            queued_zone_bytes: 64 * 1024 * 1024,
            queued_peer_bytes: 8 * 1024 * 1024,
            journal_bytes: 1024 * 1024 * 1024,
            verification_concurrency_per_peer: 32,
        }
    }
}

/// Directed route key used for independent count and value caps.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RouteKey {
    /// Source Zone ID.
    pub source_zone: u32,
    /// Destination Zone ID.
    pub destination_zone: u32,
    /// L1 token identity.
    pub l1_token: Address,
}

impl RouteKey {
    /// Derive the route from an intent.
    pub const fn from_intent(intent: &TransferIntent) -> Self {
        Self {
            source_zone: intent.source.zone_id,
            destination_zone: intent.destination.zone_id,
            l1_token: intent.asset.l1_token,
        }
    }
}

/// Explicit base-unit caps; no implicit decimal conversion is performed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValueCaps {
    /// Maximum unresolved principal on a route.
    pub route: U256,
    /// Maximum unresolved principal for a sender.
    pub sender: U256,
}

/// Admission result preserving duplicate idempotency.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionDecision {
    /// A new obligation was reserved.
    Accepted,
    /// The same transfer ID and intent body was already reserved.
    Duplicate,
}

/// Admission refusal or invariant violation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AdmissionError {
    /// Same transfer ID was presented with another body.
    #[error("transfer ID already binds a different intent")]
    ConflictingIntent,
    /// A configured resource or count cap would be crossed.
    #[error("admission limit exceeded: {0}")]
    Limit(&'static str),
    /// A checked byte or base-unit counter overflowed.
    #[error("admission accounting overflow")]
    Overflow,
    /// Route or sender value caps have not been configured.
    #[error("missing explicit value caps")]
    MissingValueCaps,
    /// Release did not identify an active reservation.
    #[error("unknown admission reservation")]
    UnknownReservation,
    /// Pool arithmetic or evidence was invalid.
    #[error("invalid pool accounting: {0}")]
    Pool(&'static str),
}

#[derive(Clone, Debug)]
struct Reservation {
    intent_hash: B256,
    route: RouteKey,
    sender: Address,
    principal: U256,
    queued_bytes: usize,
    journal_bytes: usize,
}

/// In-memory deterministic accounting view intended to be persisted transactionally by the node.
#[derive(Clone, Debug)]
pub struct AdmissionController {
    limits: ProtocolLimits,
    reservations: HashMap<B256, Reservation>,
    route_counts: HashMap<RouteKey, usize>,
    sender_counts: HashMap<Address, usize>,
    route_values: HashMap<RouteKey, U256>,
    sender_values: HashMap<Address, U256>,
    value_caps: HashMap<RouteKey, ValueCaps>,
    queued_zone_bytes: usize,
    queued_peer_bytes: HashMap<u32, usize>,
    journal_bytes: usize,
    verification_inflight: HashMap<u32, usize>,
}

impl AdmissionController {
    /// Start with zero obligations and explicit deployment limits.
    pub fn new(limits: ProtocolLimits) -> Self {
        Self {
            limits,
            reservations: HashMap::new(),
            route_counts: HashMap::new(),
            sender_counts: HashMap::new(),
            route_values: HashMap::new(),
            sender_values: HashMap::new(),
            value_caps: HashMap::new(),
            queued_zone_bytes: 0,
            queued_peer_bytes: HashMap::new(),
            journal_bytes: 0,
            verification_inflight: HashMap::new(),
        }
    }

    /// Configure explicit per-route and per-sender value caps.
    pub fn set_value_caps(&mut self, route: RouteKey, caps: ValueCaps) {
        self.value_caps.insert(route, caps);
    }

    /// Atomically reserve a new source lock obligation.
    pub fn reserve(
        &mut self,
        intent: &TransferIntent,
        queued_bytes: usize,
        journal_bytes: usize,
    ) -> Result<AdmissionDecision, AdmissionError> {
        let id = intent.transfer_id();
        let intent_hash = intent.intent_hash();
        if let Some(existing) = self.reservations.get(&id) {
            return if existing.intent_hash == intent_hash {
                Ok(AdmissionDecision::Duplicate)
            } else {
                Err(AdmissionError::ConflictingIntent)
            };
        }
        let route = RouteKey::from_intent(intent);
        let caps = self
            .value_caps
            .get(&route)
            .copied()
            .ok_or(AdmissionError::MissingValueCaps)?;
        checked_limit(
            self.reservations.len(),
            1,
            self.limits.unresolved_zone,
            "zone unresolved count",
        )?;
        checked_limit(
            *self.route_counts.get(&route).unwrap_or(&0),
            1,
            self.limits.unresolved_route,
            "route unresolved count",
        )?;
        checked_limit(
            *self.sender_counts.get(&intent.sender).unwrap_or(&0),
            1,
            self.limits.unresolved_sender,
            "sender unresolved count",
        )?;
        checked_limit(
            self.queued_zone_bytes,
            queued_bytes,
            self.limits.queued_zone_bytes,
            "zone queue bytes",
        )?;
        checked_limit(
            self.journal_bytes,
            journal_bytes,
            self.limits.journal_bytes,
            "journal bytes",
        )?;
        let route_value = checked_add_u256(
            *self.route_values.get(&route).unwrap_or(&U256::ZERO),
            intent.principal,
        )?;
        let sender_value = checked_add_u256(
            *self
                .sender_values
                .get(&intent.sender)
                .unwrap_or(&U256::ZERO),
            intent.principal,
        )?;
        if route_value > caps.route {
            return Err(AdmissionError::Limit("route base-unit exposure"));
        }
        if sender_value > caps.sender {
            return Err(AdmissionError::Limit("sender base-unit exposure"));
        }

        self.queued_zone_bytes += queued_bytes;
        self.journal_bytes += journal_bytes;
        *self.route_counts.entry(route).or_default() += 1;
        *self.sender_counts.entry(intent.sender).or_default() += 1;
        self.route_values.insert(route, route_value);
        self.sender_values.insert(intent.sender, sender_value);
        self.reservations.insert(
            id,
            Reservation {
                intent_hash,
                route,
                sender: intent.sender,
                principal: intent.principal,
                queued_bytes,
                journal_bytes,
            },
        );
        Ok(AdmissionDecision::Accepted)
    }

    /// Release resource/value accounting only after terminal source disposition is durable.
    pub fn release(&mut self, transfer_id: B256) -> Result<(), AdmissionError> {
        let reservation = self
            .reservations
            .remove(&transfer_id)
            .ok_or(AdmissionError::UnknownReservation)?;
        self.queued_zone_bytes -= reservation.queued_bytes;
        self.journal_bytes -= reservation.journal_bytes;
        decrement(&mut self.route_counts, reservation.route);
        decrement(&mut self.sender_counts, reservation.sender);
        subtract_value(
            &mut self.route_values,
            reservation.route,
            reservation.principal,
        )?;
        subtract_value(
            &mut self.sender_values,
            reservation.sender,
            reservation.principal,
        )?;
        Ok(())
    }

    /// Reserve bytes for one authenticated peer queue within both byte limits.
    pub fn reserve_peer_bytes(
        &mut self,
        peer_zone: u32,
        bytes: usize,
    ) -> Result<(), AdmissionError> {
        checked_limit(
            self.queued_zone_bytes,
            bytes,
            self.limits.queued_zone_bytes,
            "zone queue bytes",
        )?;
        checked_limit(
            *self.queued_peer_bytes.get(&peer_zone).unwrap_or(&0),
            bytes,
            self.limits.queued_peer_bytes,
            "peer queue bytes",
        )?;
        self.queued_zone_bytes += bytes;
        *self.queued_peer_bytes.entry(peer_zone).or_default() += bytes;
        Ok(())
    }

    /// Release bytes only after the durable delivery layer allows compaction.
    pub fn release_peer_bytes(
        &mut self,
        peer_zone: u32,
        bytes: usize,
    ) -> Result<(), AdmissionError> {
        let peer = self
            .queued_peer_bytes
            .get_mut(&peer_zone)
            .ok_or(AdmissionError::UnknownReservation)?;
        if *peer < bytes || self.queued_zone_bytes < bytes {
            return Err(AdmissionError::Overflow);
        }
        *peer -= bytes;
        self.queued_zone_bytes -= bytes;
        Ok(())
    }

    /// Acquire one bounded peer certificate-verification slot.
    pub fn acquire_verification(&mut self, peer_zone: u32) -> Result<(), AdmissionError> {
        let inflight = self.verification_inflight.entry(peer_zone).or_default();
        if *inflight >= self.limits.verification_concurrency_per_peer {
            return Err(AdmissionError::Limit("peer verification concurrency"));
        }
        *inflight += 1;
        Ok(())
    }

    /// Release a previously acquired verification slot.
    pub fn release_verification(&mut self, peer_zone: u32) -> Result<(), AdmissionError> {
        let inflight = self
            .verification_inflight
            .get_mut(&peer_zone)
            .ok_or(AdmissionError::UnknownReservation)?;
        if *inflight == 0 {
            return Err(AdmissionError::UnknownReservation);
        }
        *inflight -= 1;
        Ok(())
    }

    /// Number of live obligations across both directions represented by this controller.
    pub fn unresolved(&self) -> usize {
        self.reservations.len()
    }
}

fn checked_limit(
    current: usize,
    added: usize,
    limit: usize,
    name: &'static str,
) -> Result<(), AdmissionError> {
    if current.checked_add(added).ok_or(AdmissionError::Overflow)? > limit {
        Err(AdmissionError::Limit(name))
    } else {
        Ok(())
    }
}

fn checked_add_u256(left: U256, right: U256) -> Result<U256, AdmissionError> {
    left.checked_add(right).ok_or(AdmissionError::Overflow)
}

fn decrement<K: Eq + std::hash::Hash + Copy>(map: &mut HashMap<K, usize>, key: K) {
    if let Some(value) = map.get_mut(&key) {
        *value -= 1;
        if *value == 0 {
            map.remove(&key);
        }
    }
}

fn subtract_value<K: Eq + std::hash::Hash + Copy>(
    map: &mut HashMap<K, U256>,
    key: K,
    amount: U256,
) -> Result<(), AdmissionError> {
    let value = map.get_mut(&key).ok_or(AdmissionError::Overflow)?;
    *value = value.checked_sub(amount).ok_or(AdmissionError::Overflow)?;
    if value.is_zero() {
        map.remove(&key);
    }
    Ok(())
}

/// Accounting for one real-token destination pool.
#[derive(Clone, Debug)]
pub struct PoolAccounting {
    /// Token mapping this pool actually holds.
    pub asset: AssetId,
    /// Actual funded token balance tracked by native execution.
    funded_balance: U256,
    /// Balance unavailable to fast payments.
    minimum_reserve: U256,
    /// Unretired principal by source Zone.
    unsettled_by_source: HashMap<u32, U256>,
    /// Explicit source exposure caps.
    exposure_caps: HashMap<u32, U256>,
    /// Permanent idempotency markers for accepted release proofs.
    retired: HashSet<B256>,
}

impl PoolAccounting {
    /// Construct accounting from an observed real token balance.
    pub fn new(
        asset: AssetId,
        funded_balance: U256,
        minimum_reserve: U256,
    ) -> Result<Self, AdmissionError> {
        if minimum_reserve > funded_balance {
            return Err(AdmissionError::Pool("reserve exceeds funded balance"));
        }
        Ok(Self {
            asset,
            funded_balance,
            minimum_reserve,
            unsettled_by_source: HashMap::new(),
            exposure_caps: HashMap::new(),
            retired: HashSet::new(),
        })
    }

    /// Configure an exact source principal exposure cap.
    pub fn set_exposure_cap(&mut self, source_zone: u32, cap: U256) {
        self.exposure_caps.insert(source_zone, cap);
    }

    /// Spendable real balance excluding the reserve.
    pub fn spendable(&self) -> U256 {
        self.funded_balance.saturating_sub(self.minimum_reserve)
    }

    /// Atomically account a committed payment debit and its unsettled source claim.
    pub fn commit_payment(
        &mut self,
        transfer_id: B256,
        source_zone: u32,
        principal: U256,
    ) -> Result<(), AdmissionError> {
        if principal.is_zero() {
            return Err(AdmissionError::Pool("zero principal"));
        }
        if self.retired.contains(&transfer_id) {
            return Err(AdmissionError::Pool("retired transfer cannot be paid"));
        }
        if principal > self.spendable() {
            return Err(AdmissionError::Pool("insufficient spendable liquidity"));
        }
        let cap = self
            .exposure_caps
            .get(&source_zone)
            .copied()
            .ok_or(AdmissionError::Pool("missing source exposure cap"))?;
        let exposure = checked_add_u256(
            *self
                .unsettled_by_source
                .get(&source_zone)
                .unwrap_or(&U256::ZERO),
            principal,
        )?;
        if exposure > cap {
            return Err(AdmissionError::Pool("source exposure cap"));
        }
        self.funded_balance = self
            .funded_balance
            .checked_sub(principal)
            .ok_or(AdmissionError::Overflow)?;
        self.unsettled_by_source.insert(source_zone, exposure);
        Ok(())
    }

    /// Credit only an actual observed pool token increase.
    pub fn credit_observed(&mut self, net_amount: U256) -> Result<(), AdmissionError> {
        self.funded_balance = checked_add_u256(self.funded_balance, net_amount)?;
        Ok(())
    }

    /// Retire source exposure once for verified finalized release evidence.
    pub fn retire_exposure(
        &mut self,
        transfer_id: B256,
        source_zone: u32,
        principal: U256,
    ) -> Result<bool, AdmissionError> {
        if self.retired.contains(&transfer_id) {
            return Ok(false);
        }
        let exposure = self
            .unsettled_by_source
            .get_mut(&source_zone)
            .ok_or(AdmissionError::Pool("no source exposure"))?;
        *exposure = exposure
            .checked_sub(principal)
            .ok_or(AdmissionError::Pool("retirement exceeds exposure"))?;
        if exposure.is_zero() {
            self.unsettled_by_source.remove(&source_zone);
        }
        self.retired.insert(transfer_id);
        Ok(true)
    }

    /// Current real token balance (not claims or in-flight deposits).
    pub const fn funded_balance(&self) -> U256 {
        self.funded_balance
    }
}

#[cfg(test)]
mod tests {
    use zone_primitives::fast_transfer::{AssetId, ZoneDomain};

    use super::*;

    fn intent(nonce: u64) -> TransferIntent {
        let domain = |zone, byte| ZoneDomain {
            l1_chain_id: 1,
            zone_id: zone,
            chain_id: zone as u64 + 100,
            portal: Address::repeat_byte(byte),
            authority_epoch: 1,
            roster_hash: B256::repeat_byte(byte),
            protocol_version: 1,
        };
        TransferIntent {
            source: domain(1, 1),
            destination: domain(2, 2),
            asset: AssetId {
                l1_token: Address::repeat_byte(3),
                source_token: Address::repeat_byte(4),
                destination_token: Address::repeat_byte(5),
                decimals: 6,
            },
            sender: Address::repeat_byte(6),
            recipient: Address::repeat_byte(7),
            refund_account: Address::repeat_byte(6),
            destination_pool: Address::repeat_byte(8),
            reimbursement_account: Address::repeat_byte(9),
            principal: U256::from(10),
            fee: U256::ZERO,
            quote_id: B256::repeat_byte(10),
            destination_expiry_height: 100,
            transfer_nonce: nonce,
        }
    }

    #[test]
    fn duplicates_do_not_consume_capacity() {
        let mut admission = AdmissionController::new(ProtocolLimits {
            unresolved_zone: 1,
            ..ProtocolLimits::default()
        });
        let first = intent(1);
        admission.set_value_caps(
            RouteKey::from_intent(&first),
            ValueCaps {
                route: U256::from(20),
                sender: U256::from(20),
            },
        );
        assert_eq!(
            admission.reserve(&first, 10, 20),
            Ok(AdmissionDecision::Accepted)
        );
        assert_eq!(
            admission.reserve(&first, 10, 20),
            Ok(AdmissionDecision::Duplicate)
        );
        assert!(matches!(
            admission.reserve(&intent(2), 10, 20),
            Err(AdmissionError::Limit(_))
        ));
    }

    #[test]
    fn pool_never_spends_reserve_and_retires_once() {
        let asset = intent(1).asset;
        let mut pool = PoolAccounting::new(asset, U256::from(11), U256::from(1)).unwrap();
        pool.set_exposure_cap(1, U256::from(10));
        let id = B256::repeat_byte(1);
        pool.commit_payment(id, 1, U256::from(10)).unwrap();
        assert_eq!(pool.funded_balance(), U256::from(1));
        assert!(
            pool.commit_payment(B256::repeat_byte(2), 1, U256::from(1))
                .is_err()
        );
        assert_eq!(pool.retire_exposure(id, 1, U256::from(10)), Ok(true));
        assert_eq!(pool.retire_exposure(id, 1, U256::from(10)), Ok(false));
    }
}
