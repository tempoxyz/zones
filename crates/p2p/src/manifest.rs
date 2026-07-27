use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
};

use alloy_primitives::Address as EthereumAddress;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{Address, Ingress};
use commonware_utils::Hostname;
use derive_more::{Display, FromStr};
use serde::Deserialize;

/// The role assigned to a node by the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, FromStr)]
#[display(rename_all = "lowercase")]
#[from_str(rename_all = "lowercase")]
pub enum Role {
    /// Builds blocks and runs the existing sequencer settlement tasks.
    Leader,
    /// Runs without block production, follows `Leader`s blocks
    Follower,
}

/// One finalized leadership transition.
///
/// A record authorizes `leader` for every Tempo anchor with
/// `anchor >= activation_tempo_block`, until a later transition's activation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadershipState {
    /// Monotonically increasing fencing epoch.
    pub epoch: u64,
    /// Ed25519 identity of the selected leader.
    pub leader: PublicKey,
    /// Tempo (L1) block number at which this leader's authorization begins.
    pub activation_tempo_block: u64,
}

impl LeadershipState {
    /// Creates a leadership record.
    pub const fn new(epoch: u64, leader: PublicKey, activation_tempo_block: u64) -> Self {
        Self {
            epoch,
            leader,
            activation_tempo_block,
        }
    }

    /// Monotonically increasing fencing epoch.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// First Tempo anchor governed by this record.
    pub const fn activation_tempo_block(&self) -> u64 {
        self.activation_tempo_block
    }

    /// Leader selected by this record.
    pub const fn leader(&self) -> &PublicKey {
        &self.leader
    }

    /// Returns the role of `ed25519_public_key` under this record.
    pub fn role_of(&self, ed25519_public_key: &PublicKey) -> Role {
        if ed25519_public_key == &self.leader {
            Role::Leader
        } else {
            Role::Follower
        }
    }
}

#[derive(Debug, Default)]
struct LeadershipScheduleState {
    /// Retained transitions indexed by activation Tempo block.
    transitions: std::collections::BTreeMap<u64, LeadershipState>,
    /// Highest epoch finalized L1 has shown us (observability + publication check).
    latest_observed_epoch: Option<u64>,
    /// Highest Tempo anchor embedded in a locally canonical zone block.
    applied_anchor: Option<u64>,
}

/// Activation-indexed schedule of finalized leadership transitions.
///
/// Every observed leadership transition is kept
/// until the applied Tempo checkpoint passes activation boundary. A watch notifier announces
/// changes, but consumers re-read and index by anchor.
///
/// An empty schedule represents the "uninitialized" state: no portal leader has been
/// observed at the local Tempo checkpoint and block production must stay off.
#[derive(Debug, Clone)]
pub struct LeadershipSchedule {
    inner: std::sync::Arc<std::sync::RwLock<LeadershipScheduleState>>,
    changed: tokio::sync::watch::Sender<()>,
}

impl Default for LeadershipSchedule {
    fn default() -> Self {
        Self::uninitialized()
    }
}

impl LeadershipSchedule {
    /// Creates an empty (fenced, uninitialized) schedule.
    pub fn uninitialized() -> Self {
        let (changed, _) = tokio::sync::watch::channel(());
        Self {
            inner: std::sync::Arc::new(std::sync::RwLock::new(LeadershipScheduleState::default())),
            changed,
        }
    }

    /// Creates a schedule seeded with one record.
    pub fn seeded(initial: LeadershipState) -> Self {
        let schedule = Self::uninitialized();
        schedule
            .publish(initial)
            .expect("seeding an empty schedule cannot conflict");
        schedule
    }

    /// Returns whether any leadership record has been observed.
    pub fn is_initialized(&self) -> bool {
        !self.inner.read().expect("poisoned").transitions.is_empty()
    }

    /// Publish a transition observed from verified finalized receipts (or a startup snapshot).
    ///
    /// Publication is checked: an identical re-observation (subscriber replay) is an `Ok`
    /// no-op, while a conflicting record at an observed activation, a non-monotonic epoch, or
    /// a non-monotonic activation is an error that must fence ingestion in the caller.
    /// Returns whether a new transition was appended.
    pub fn publish(&self, record: LeadershipState) -> eyre::Result<bool> {
        let mut state = self.inner.write().expect("poisoned");
        // The first observed record is the earliest known authority and governs from anchor
        // zero: a fresh zone's genesis anchors precede the portal creation block, and no
        // other producer can exist before the first transition.
        let record = if state.transitions.is_empty() {
            LeadershipState {
                activation_tempo_block: 0,
                ..record
            }
        } else {
            record
        };
        if let Some(existing) = state.transitions.get(&record.activation_tempo_block) {
            eyre::ensure!(
                *existing == record,
                "conflicting leadership transition at activation {}: existing epoch {} leader \
                 {}, new epoch {} leader {}",
                record.activation_tempo_block,
                existing.epoch,
                existing.leader,
                record.epoch,
                record.leader,
            );
            return Ok(false);
        }
        if let Some((&last_activation, last)) = state.transitions.last_key_value() {
            if record.epoch == last.epoch {
                // A re-observation of the clamped initial record at its true activation is
                // the same authority; anything else with a duplicate epoch is corrupt.
                if record.leader == last.leader
                    && last_activation == 0
                    && state.transitions.len() == 1
                {
                    return Ok(false);
                }
                eyre::bail!(
                    "duplicate leadership epoch {} at a different activation: retained {}, new {}",
                    record.epoch,
                    last_activation,
                    record.activation_tempo_block,
                );
            }
            eyre::ensure!(
                record.epoch == last.epoch + 1,
                "non-contiguous leadership epoch: retained {}, new {}",
                last.epoch,
                record.epoch,
            );
            eyre::ensure!(
                record.activation_tempo_block > last_activation,
                "leadership activation moved backwards: retained {}, new {}",
                last_activation,
                record.activation_tempo_block,
            );
        }
        state.latest_observed_epoch = Some(
            state
                .latest_observed_epoch
                .map_or(record.epoch, |epoch| epoch.max(record.epoch)),
        );
        state
            .transitions
            .insert(record.activation_tempo_block, record);
        drop(state);
        self.changed.send_replace(());
        Ok(true)
    }

    /// Returns the record governing `tempo_anchor`: the greatest activation `<= tempo_anchor`
    /// over every retained transition. `None` while uninitialized or for anchors before the
    /// first retained activation — both fence production and import for that anchor.
    pub fn leader_for(&self, tempo_anchor: u64) -> Option<LeadershipState> {
        self.inner
            .read()
            .expect("poisoned")
            .transitions
            .range(..=tempo_anchor)
            .next_back()
            .map(|(_, record)| record.clone())
    }

    /// Returns the most recently observed record (status only, never a production permit).
    pub fn latest(&self) -> Option<LeadershipState> {
        self.inner
            .read()
            .expect("poisoned")
            .transitions
            .last_key_value()
            .map(|(_, record)| record.clone())
    }

    /// Returns the record governing the next anchor this node will consume
    /// (`applied_anchor + 1`), falling back to the most recent record while no applied
    /// anchor has been recorded yet.
    ///
    /// This is the outbound routing authority for anchor-bound traffic such as transaction
    /// forwarding: between observing a transition and reaching its activation boundary, the
    /// outgoing leader — not the most recently observed one — still produces. Routing only,
    /// never a production permit.
    pub fn next_anchor_record(&self) -> Option<LeadershipState> {
        let state = self.inner.read().expect("poisoned");
        let next_anchor = state
            .applied_anchor
            .map_or(u64::MAX, |applied| applied.saturating_add(1));
        state
            .transitions
            .range(..=next_anchor)
            .next_back()
            .map(|(_, record)| record.clone())
    }

    /// Highest epoch finalized L1 has shown us.
    pub fn latest_observed_epoch(&self) -> Option<u64> {
        self.inner.read().expect("poisoned").latest_observed_epoch
    }

    /// Epoch whose activation boundary the locally applied checkpoint has crossed.
    ///
    /// Observability only — promotion keys off `leader_for(next anchor)`, never this value.
    pub fn locally_applied_epoch(&self) -> Option<u64> {
        let state = self.inner.read().expect("poisoned");
        let applied = state.applied_anchor?;
        state
            .transitions
            .range(..=applied)
            .next_back()
            .map(|(_, record)| record.epoch)
    }

    /// Number of observed transitions whose activation the applied checkpoint has not crossed.
    pub fn pending_transitions(&self) -> usize {
        let state = self.inner.read().expect("poisoned");
        let applied = state.applied_anchor.unwrap_or(0);
        state.transitions.range(applied + 1..).count()
    }

    /// Record that the zone block embedding `tempo_anchor` is locally canonical, pruning
    /// entries whose boundary the checkpoint has passed.
    ///
    /// The entry governing the next anchor is always retained, along with its immediate
    /// predecessor: the promotion barrier distinguishes a planned handoff from same-identity
    /// recovery by asking who governed the previous anchor.
    pub fn record_applied_anchor(&self, tempo_anchor: u64) {
        let mut state = self.inner.write().expect("poisoned");
        state.applied_anchor = Some(
            state
                .applied_anchor
                .map_or(tempo_anchor, |applied| applied.max(tempo_anchor)),
        );
        let applied = state.applied_anchor.expect("just set");
        // The entry governing the next anchor is the greatest activation <= applied + 1.
        if let Some((&active, _)) = state
            .transitions
            .range(..=applied.saturating_add(1))
            .next_back()
        {
            let keep_from = state
                .transitions
                .range(..active)
                .next_back()
                .map_or(active, |(&predecessor, _)| predecessor);
            state
                .transitions
                .retain(|&activation, _| activation >= keep_from);
        }
        drop(state);
        self.changed.send_replace(());
    }

    /// Subscribe to schedule-change notifications. The watch is a wakeup, not the schedule:
    /// consumers re-read via [`Self::leader_for`].
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Returns the role of `ed25519_public_key` for `tempo_anchor`, when governed.
    pub fn role_for(&self, ed25519_public_key: &PublicKey, tempo_anchor: u64) -> Option<Role> {
        self.leader_for(tempo_anchor)
            .map(|record| record.role_of(ed25519_public_key))
    }

    /// Returns whether `ed25519_public_key` leads any retained transition.
    ///
    /// A transport-level acceptance check for live blocks: a lagging follower must keep
    /// accepting the rightful producer of in-between anchors after a later transition is
    /// observed. The exact per-anchor fence lives in the import path.
    pub fn is_scheduled_leader(&self, ed25519_public_key: &PublicKey) -> bool {
        self.inner
            .read()
            .expect("poisoned")
            .transitions
            .values()
            .any(|record| &record.leader == ed25519_public_key)
    }
}

/// A validated P2P address from the manifest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManifestAddress {
    /// A literal IP address and port.
    Socket(SocketAddr),
    /// A DNS hostname and port.
    Dns { host: Hostname, port: u16 },
}

impl ManifestAddress {
    pub(crate) fn to_commonware(&self) -> Address {
        match self {
            Self::Socket(address) => Address::Symmetric(*address),
            Self::Dns { host, port } => Address::Asymmetric {
                ingress: Ingress::Dns {
                    host: host.clone(),
                    port: *port,
                },
                // DNS-only manifests cannot know a pod's egress IP ahead of time.
                // The network enables authenticated-key-only ingress filtering when
                // DNS addresses are present, so this placeholder is never consulted.
                egress: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            },
        }
    }

    pub(crate) const fn is_dns(&self) -> bool {
        matches!(self, Self::Dns { .. })
    }
}

impl fmt::Display for ManifestAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(address) => address.fmt(f),
            Self::Dns { host, port } => write!(f, "{host}:{port}"),
        }
    }
}

impl std::str::FromStr for ManifestAddress {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Ok(address) = value.parse::<SocketAddr>() {
            if address.port() == 0 {
                return Err("port must be non-zero".to_owned());
            }
            return Ok(Self::Socket(address));
        }

        let (host, port) = value
            .rsplit_once(':')
            .ok_or_else(|| "expected `host:port`".to_owned())?;
        if host.is_empty() || host.contains(':') {
            return Err("expected a DNS hostname or bracketed IP followed by a port".to_owned());
        }
        let host = Hostname::new(host).map_err(|err| format!("invalid DNS hostname: {err}"))?;
        let port = port
            .parse::<u16>()
            .map_err(|err| format!("invalid port: {err}"))?;
        if port == 0 {
            return Err("port must be non-zero".to_owned());
        }
        Ok(Self::Dns { host, port })
    }
}

/// One node in a zone's static multi-sequencer topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestNode {
    name: String,
    ed25519_public_key: PublicKey,
    secp256k1_address: EthereumAddress,
    address: ManifestAddress,
}

impl ManifestNode {
    /// Human-readable node name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Node's Ed25519 public key used to authenticate Commonware traffic.
    pub const fn ed25519_public_key(&self) -> &PublicKey {
        &self.ed25519_public_key
    }

    /// Address derived from this node's individual secp256k1 key.
    pub const fn secp256k1_address(&self) -> EthereumAddress {
        self.secp256k1_address
    }

    /// Node's advertised P2P address.
    pub const fn address(&self) -> &ManifestAddress {
        &self.address
    }
}

/// A parsed and intrinsically validated zone manifest.
#[derive(Debug, Clone)]
pub struct ZoneManifest {
    zone_id: u32,
    sequencer_set_version: u64,
    leader_ed25519_public_key: PublicKey,
    nodes: Vec<ManifestNode>,
}

impl ZoneManifest {
    /// Parses and validates a TOML manifest.
    pub fn parse(input: &str) -> Result<Self, ManifestError> {
        let raw: RawManifest = toml::from_str(input).map_err(ManifestError::Toml)?;
        if raw.sequencer_set_version == 0 {
            return Err(ManifestError::InvalidSequencerSetVersion);
        }
        if raw.nodes.len() < 3 {
            return Err(ManifestError::TooFewNodes(raw.nodes.len()));
        }

        let leader_ed25519_public_key =
            parse_ed25519_public_key("leader_ed25519_public_key", &raw.leader_ed25519_public_key)?;
        let mut names = BTreeSet::new();
        let mut ed25519_public_keys = BTreeSet::new();
        let mut secp256k1_addresses = BTreeSet::new();
        let mut nodes = Vec::with_capacity(raw.nodes.len());

        for raw_node in raw.nodes {
            if raw_node.name.trim().is_empty() {
                return Err(ManifestError::EmptyNodeName);
            }
            if !names.insert(raw_node.name.clone()) {
                return Err(ManifestError::DuplicateNodeName(raw_node.name));
            }

            let ed25519_public_key = parse_ed25519_public_key(
                &format!("nodes.{}.ed25519_public_key", raw_node.name),
                &raw_node.ed25519_public_key,
            )?;
            if !ed25519_public_keys.insert(ed25519_public_key.clone()) {
                return Err(ManifestError::DuplicateEd25519PublicKey(
                    ed25519_public_key.to_string(),
                ));
            }

            let secp256k1_address = raw_node
                .secp256k1_address
                .parse::<EthereumAddress>()
                .map_err(|source| ManifestError::InvalidSecp256k1Address {
                    node: raw_node.name.clone(),
                    address: raw_node.secp256k1_address.clone(),
                    reason: source.to_string(),
                })?;
            if !secp256k1_addresses.insert(secp256k1_address) {
                return Err(ManifestError::DuplicateSecp256k1Address(secp256k1_address));
            }

            let address =
                raw_node
                    .address
                    .parse()
                    .map_err(|reason| ManifestError::InvalidAddress {
                        node: raw_node.name.clone(),
                        address: raw_node.address.clone(),
                        reason,
                    })?;
            nodes.push(ManifestNode {
                name: raw_node.name,
                ed25519_public_key,
                secp256k1_address,
                address,
            });
        }

        if !ed25519_public_keys.contains(&leader_ed25519_public_key) {
            return Err(ManifestError::LeaderEd25519PublicKeyNotFound(
                leader_ed25519_public_key.to_string(),
            ));
        }

        Ok(Self {
            zone_id: raw.zone_id,
            sequencer_set_version: raw.sequencer_set_version,
            leader_ed25519_public_key,
            nodes,
        })
    }

    /// Reads, parses, and validates a TOML manifest from disk.
    pub fn read_from_file(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let path = path.as_ref();
        let input = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&input)
    }

    /// Validates node-specific invariants and returns the manifest-derived role.
    pub fn validate_node(
        &self,
        expected_zone_id: u32,
        local_ed25519_public_key: &PublicKey,
        local_secp256k1_address: EthereumAddress,
        asserted_role: Option<Role>,
    ) -> Result<Role, ManifestError> {
        if self.zone_id != expected_zone_id {
            return Err(ManifestError::ZoneIdMismatch {
                manifest: self.zone_id,
                cli: expected_zone_id,
            });
        }

        let local_node = self
            .node_by_ed25519_public_key(local_ed25519_public_key)
            .ok_or_else(|| {
                ManifestError::LocalNodeNotFound(local_ed25519_public_key.to_string())
            })?;
        if local_node.secp256k1_address != local_secp256k1_address {
            return Err(ManifestError::LocalSecp256k1AddressMismatch {
                manifest: local_node.secp256k1_address,
                local: local_secp256k1_address,
            });
        }
        let role = self
            .bootstrap_leadership()
            .role_of(local_ed25519_public_key);
        if let Some(asserted) = asserted_role
            && asserted != role
        {
            return Err(ManifestError::RoleMismatch {
                asserted,
                manifest: role,
            });
        }
        Ok(role)
    }

    /// Zone identifier used to domain-separate the P2P network.
    pub const fn zone_id(&self) -> u32 {
        self.zone_id
    }

    /// Version of the registered L1 attester set used in EIP-712 statements.
    pub const fn sequencer_set_version(&self) -> u64 {
        self.sequencer_set_version
    }

    /// Ed25519 Commonware public key of the configured initial leader.
    pub const fn leader_ed25519_public_key(&self) -> &PublicKey {
        &self.leader_ed25519_public_key
    }

    /// Manifest-derived initial leadership record (epoch 0, active from genesis).
    pub fn bootstrap_leadership(&self) -> LeadershipState {
        LeadershipState::new(0, self.leader_ed25519_public_key.clone(), 0)
    }

    /// All nodes in the static peer set.
    pub fn nodes(&self) -> &[ManifestNode] {
        &self.nodes
    }

    /// Returns whether an Ed25519 key belongs to the manifest's static peer set.
    pub fn contains_ed25519_public_key(&self, ed25519_public_key: &PublicKey) -> bool {
        self.node_by_ed25519_public_key(ed25519_public_key)
            .is_some()
    }

    /// Returns the manifest node registered with an Ed25519 public key.
    pub fn node_by_ed25519_public_key(
        &self,
        ed25519_public_key: &PublicKey,
    ) -> Option<&ManifestNode> {
        self.nodes
            .iter()
            .find(|node| node.ed25519_public_key() == ed25519_public_key)
    }

    /// Returns the manifest node registered with an individual secp256k1 address.
    pub fn node_by_secp256k1_address(
        &self,
        secp256k1_address: EthereumAddress,
    ) -> Option<&ManifestNode> {
        self.nodes
            .iter()
            .find(|node| node.secp256k1_address() == secp256k1_address)
    }

    pub(crate) fn has_dns_addresses(&self) -> bool {
        self.nodes.iter().any(|node| node.address.is_dns())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    zone_id: u32,
    #[serde(default = "default_sequencer_set_version")]
    sequencer_set_version: u64,
    leader_ed25519_public_key: String,
    nodes: Vec<RawManifestNode>,
}

const fn default_sequencer_set_version() -> u64 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifestNode {
    name: String,
    ed25519_public_key: String,
    secp256k1_address: String,
    address: String,
}

fn parse_ed25519_public_key(field: &str, encoded: &str) -> Result<PublicKey, ManifestError> {
    let bytes =
        const_hex::decode(encoded).map_err(|source| ManifestError::InvalidEd25519PublicKey {
            field: field.to_owned(),
            reason: source.to_string(),
        })?;
    PublicKey::decode(&bytes[..]).map_err(|source| ManifestError::InvalidEd25519PublicKey {
        field: field.to_owned(),
        reason: source.to_string(),
    })
}

/// Manifest parsing and validation errors.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("sequencer_set_version must be non-zero")]
    InvalidSequencerSetVersion,

    #[error("failed reading sequencer manifest `{path}`")]
    Read {
        path: std::path::PathBuf,

        #[source]
        source: std::io::Error,
    },

    #[error("invalid sequencer manifest TOML")]
    Toml(#[source] toml::de::Error),

    #[error("sequencer manifest must contain at least 3 nodes, found {0}")]
    TooFewNodes(usize),

    #[error("sequencer manifest node names must not be empty")]
    EmptyNodeName,

    #[error("duplicate sequencer manifest node name `{0}`")]
    DuplicateNodeName(String),

    #[error("duplicate sequencer manifest Ed25519 public key `{0}`")]
    DuplicateEd25519PublicKey(String),

    #[error("duplicate sequencer manifest secp256k1 address `{0}`")]
    DuplicateSecp256k1Address(EthereumAddress),

    #[error("invalid Ed25519 public key in `{field}`: {reason}")]
    InvalidEd25519PublicKey { field: String, reason: String },

    #[error("invalid secp256k1 address `{address}` for node `{node}`: {reason}")]
    InvalidSecp256k1Address {
        node: String,
        address: String,
        reason: String,
    },

    #[error("invalid address `{address}` for node `{node}`: {reason}")]
    InvalidAddress {
        node: String,
        address: String,
        reason: String,
    },

    #[error("manifest leader Ed25519 public key `{0}` does not match any node")]
    LeaderEd25519PublicKeyNotFound(String),

    #[error("zone ID mismatch: manifest has {manifest}, but --zone.id is {cli}")]
    ZoneIdMismatch { manifest: u32, cli: u32 },

    #[error("this node's Ed25519 public key `{0}` is not present in the sequencer manifest")]
    LocalNodeNotFound(String),

    #[error(
        "this node's secp256k1 address `{local}` does not match its manifest address `{manifest}`"
    )]
    LocalSecp256k1AddressMismatch {
        manifest: EthereumAddress,
        local: EthereumAddress,
    },

    #[error("--sequencer.role asserts `{asserted}`, but the manifest assigns `{manifest}`")]
    RoleMismatch { asserted: Role, manifest: Role },
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::{LeadershipSchedule, LeadershipState, ManifestError, Role, ZoneManifest};

    fn public_key(seed: u64) -> commonware_cryptography::ed25519::PublicKey {
        PrivateKey::from_seed(seed).public_key()
    }

    #[test]
    fn uninitialized_schedule_stays_fenced() {
        let schedule = LeadershipSchedule::uninitialized();
        assert!(!schedule.is_initialized());
        assert_eq!(schedule.leader_for(0), None);
        assert_eq!(schedule.leader_for(u64::MAX), None);
        assert_eq!(schedule.latest(), None);
        assert_eq!(schedule.latest_observed_epoch(), None);
        assert_eq!(schedule.locally_applied_epoch(), None);
        assert_eq!(schedule.role_for(&public_key(1), 100), None);
    }

    #[test]
    fn schedule_retains_skipped_intermediate_transitions() {
        // A -> B at anchor 100 and B -> C at anchor 200 both observed while the engine is
        // still before 100. leader_for over the retained timeline must name B for the
        // in-between anchors, and A below the first boundary it retains.
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(1, public_key(1), 0));
        assert!(
            schedule
                .publish(LeadershipState::new(2, public_key(2), 100))
                .unwrap()
        );
        assert!(
            schedule
                .publish(LeadershipState::new(3, public_key(3), 200))
                .unwrap()
        );

        assert_eq!(schedule.leader_for(99).unwrap().leader, public_key(1));
        assert_eq!(schedule.leader_for(100).unwrap().leader, public_key(2));
        assert_eq!(schedule.leader_for(199).unwrap().leader, public_key(2));
        assert_eq!(schedule.leader_for(200).unwrap().leader, public_key(3));
        assert_eq!(schedule.leader_for(u64::MAX).unwrap().leader, public_key(3));
        assert_eq!(schedule.latest_observed_epoch(), Some(3));
        assert_eq!(schedule.pending_transitions(), 2);
    }

    #[test]
    fn publish_is_idempotent_and_rejects_conflicts() {
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(1, public_key(1), 0));
        let transition = LeadershipState::new(2, public_key(2), 100);
        assert!(schedule.publish(transition.clone()).unwrap());
        // Subscriber replay of the same finalized block re-observes the same event.
        assert!(!schedule.publish(transition).unwrap());

        // A different leader at an observed activation is corrupt.
        assert!(
            schedule
                .publish(LeadershipState::new(2, public_key(3), 100))
                .is_err()
        );
        // Skipped epochs cannot be published: replay is contiguous.
        assert!(
            schedule
                .publish(LeadershipState::new(4, public_key(3), 200))
                .is_err()
        );
        // Activation boundaries are strictly increasing.
        assert!(
            schedule
                .publish(LeadershipState::new(3, public_key(3), 50))
                .is_err()
        );
    }

    #[test]
    fn prunes_only_past_the_applied_boundary() {
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(1, public_key(1), 0));
        schedule
            .publish(LeadershipState::new(2, public_key(2), 100))
            .unwrap();
        schedule
            .publish(LeadershipState::new(3, public_key(3), 200))
            .unwrap();

        // Applying anchors below the first boundary keeps every transition queryable.
        schedule.record_applied_anchor(98);
        assert_eq!(schedule.leader_for(99).unwrap().leader, public_key(1));
        assert_eq!(schedule.locally_applied_epoch(), Some(1));

        // Applying 99 makes the next anchor 100. Epoch 1 has served its last anchor, but it
        // is retained as the active entry's predecessor for the promotion-mode decision.
        schedule.record_applied_anchor(99);
        assert_eq!(schedule.leader_for(99).unwrap().leader, public_key(1));
        assert_eq!(schedule.leader_for(100).unwrap().leader, public_key(2));
        assert_eq!(schedule.leader_for(199).unwrap().leader, public_key(2));

        // The applied cursor never moves backwards.
        schedule.record_applied_anchor(50);
        assert_eq!(schedule.leader_for(100).unwrap().leader, public_key(2));

        schedule.record_applied_anchor(250);
        // Epoch 3 is active for anchor 251; epoch 2 is its retained predecessor; epoch 1 is
        // finally pruned.
        assert_eq!(schedule.leader_for(99), None);
        assert_eq!(schedule.leader_for(199).unwrap().leader, public_key(2));
        assert_eq!(schedule.leader_for(251).unwrap().leader, public_key(3));
        assert_eq!(schedule.locally_applied_epoch(), Some(3));
        assert_eq!(schedule.pending_transitions(), 0);
        // The active entry is always retained.
        assert!(schedule.latest().is_some());
    }

    #[test]
    fn first_record_governs_from_genesis() {
        // The creation transition activates at the portal creation block, but the zone's
        // first blocks embed earlier anchors (zone genesis anchors before createZone). The
        // earliest known authority governs them.
        let schedule = LeadershipSchedule::uninitialized();
        assert!(
            schedule
                .publish(LeadershipState::new(1, public_key(1), 500))
                .unwrap()
        );
        assert_eq!(schedule.leader_for(1).unwrap().leader, public_key(1));
        assert_eq!(schedule.leader_for(499).unwrap().leader, public_key(1));

        // Re-observing the same initial record at its true activation is idempotent.
        assert!(
            !schedule
                .publish(LeadershipState::new(1, public_key(1), 500))
                .unwrap()
        );
        // A different leader with the same epoch is still corrupt.
        assert!(
            schedule
                .publish(LeadershipState::new(1, public_key(2), 500))
                .is_err()
        );

        // Later transitions activate exactly at their boundary.
        schedule
            .publish(LeadershipState::new(2, public_key(2), 700))
            .unwrap();
        assert_eq!(schedule.leader_for(699).unwrap().leader, public_key(1));
        assert_eq!(schedule.leader_for(700).unwrap().leader, public_key(2));
    }

    #[test]
    fn schedule_watch_announces_changes() {
        let schedule = LeadershipSchedule::uninitialized();
        let watcher = schedule.subscribe();
        assert!(!watcher.has_changed().unwrap());
        schedule
            .publish(LeadershipState::new(1, public_key(1), 0))
            .unwrap();
        assert!(watcher.has_changed().unwrap());
    }

    fn ed25519_public_key(seed: u64) -> String {
        let key = PrivateKey::from_seed(seed).public_key();
        const_hex::encode_prefixed(key.as_ref())
    }

    fn secp256k1_address(seed: u64) -> String {
        format!("0x{seed:040x}")
    }

    fn manifest(leader: u64, nodes: &[(u64, &str, &str)]) -> String {
        let mut value = format!(
            "zone_id = 7\nleader_ed25519_public_key = \"{}\"\n",
            ed25519_public_key(leader)
        );
        for (key, name, address) in nodes {
            value.push_str(&format!(
                "\n[[nodes]]\nname = \"{name}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"{}\"\naddress = \"{address}\"\n",
                ed25519_public_key(*key),
                secp256k1_address(*key),
            ));
        }
        value
    }

    #[test]
    fn parses_and_derives_roles() {
        let input = manifest(
            1,
            &[
                (1, "leader", "127.0.0.1:9200"),
                (2, "follower-a", "follower-a.zone.local:9200"),
                (3, "follower-b", "127.0.0.1:9202"),
            ],
        );
        let manifest = ZoneManifest::parse(&input).unwrap();
        let leader = PrivateKey::from_seed(1).public_key();
        let follower = PrivateKey::from_seed(2).public_key();
        let leadership = manifest.bootstrap_leadership();

        assert_eq!(leadership.epoch(), 0);
        assert_eq!(leadership.activation_tempo_block(), 0);
        assert_eq!(leadership.role_of(&leader), Role::Leader);
        assert_eq!(leadership.role_of(&follower), Role::Follower);

        let schedule = super::LeadershipSchedule::seeded(leadership);
        assert_eq!(
            schedule.leader_for(0).map(|record| record.leader),
            Some(leader.clone())
        );
        assert_eq!(
            schedule.leader_for(u64::MAX).map(|record| record.leader),
            Some(leader.clone())
        );
        assert_eq!(
            manifest
                .validate_node(7, &leader, secp256k1_address(1).parse().unwrap(), None)
                .unwrap(),
            Role::Leader
        );
        assert_eq!(
            manifest
                .validate_node(
                    7,
                    &follower,
                    secp256k1_address(2).parse().unwrap(),
                    Some(Role::Follower),
                )
                .unwrap(),
            Role::Follower
        );
    }

    #[test]
    fn readme_manifest_example_has_expected_shape() {
        let (_, after_fence) = include_str!("../README.md")
            .split_once("```toml\n")
            .expect("README contains a TOML manifest example");
        let (example, _) = after_fence
            .split_once("\n```")
            .expect("README closes the TOML manifest example");

        let manifest: super::RawManifest = toml::from_str(example).unwrap();
        assert_eq!(manifest.zone_id, 7);
        assert_eq!(manifest.nodes.len(), 3);
    }

    #[test]
    fn rejects_invalid_topologies_and_node_assertions() {
        let too_small = manifest(
            1,
            &[
                (1, "leader", "127.0.0.1:9200"),
                (2, "follower", "127.0.0.1:9201"),
            ],
        );
        assert!(matches!(
            ZoneManifest::parse(&too_small),
            Err(ManifestError::TooFewNodes(2))
        ));

        let duplicate = manifest(
            1,
            &[
                (1, "leader", "127.0.0.1:9200"),
                (2, "follower", "127.0.0.1:9201"),
                (2, "follower-2", "127.0.0.1:9202"),
            ],
        );
        assert!(matches!(
            ZoneManifest::parse(&duplicate),
            Err(ManifestError::DuplicateEd25519PublicKey(_))
        ));

        let valid = ZoneManifest::parse(&manifest(
            1,
            &[
                (1, "leader", "127.0.0.1:9200"),
                (2, "follower-a", "127.0.0.1:9201"),
                (3, "follower-b", "127.0.0.1:9202"),
            ],
        ))
        .unwrap();
        let follower = PrivateKey::from_seed(2).public_key();
        assert!(matches!(
            valid.validate_node(
                7,
                &follower,
                secp256k1_address(2).parse().unwrap(),
                Some(Role::Leader),
            ),
            Err(ManifestError::RoleMismatch { .. })
        ));
        assert!(matches!(
            valid.validate_node(8, &follower, secp256k1_address(2).parse().unwrap(), None,),
            Err(ManifestError::ZoneIdMismatch { .. })
        ));
        let unknown = PrivateKey::from_seed(99).public_key();
        assert!(matches!(
            valid.validate_node(7, &unknown, secp256k1_address(99).parse().unwrap(), None,),
            Err(ManifestError::LocalNodeNotFound(_))
        ));

        assert!(matches!(
            valid.validate_node(7, &follower, secp256k1_address(3).parse().unwrap(), None,),
            Err(ManifestError::LocalSecp256k1AddressMismatch { .. })
        ));
    }
}
