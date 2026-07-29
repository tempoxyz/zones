use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
};

use alloy_primitives::{Address as EthereumAddress, B256};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{Address, Ingress};
use commonware_utils::Hostname;
use derive_more::{Display, FromStr};
use serde::Deserialize;

/// Minimum number of nodes that must be registered for the on-chain settlement quorum.
const MIN_QUORUM_NODES: usize = 3;

/// The role a node holds for a given Tempo anchor.
///
/// Which member *leads* comes from finalized L1 state; whether a member belongs to the
/// on-chain settlement quorum comes from the manifest and never changes at runtime.
///
/// `kebab-case` rather than `lowercase` so `RpcFollower` spells `rpc-follower` on the
/// `--sequencer.role` CLI flag instead of `rpcfollower`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, FromStr)]
#[display(rename_all = "kebab-case")]
#[from_str(rename_all = "kebab-case")]
pub enum Role {
    /// Builds blocks and runs the existing sequencer settlement tasks.
    Leader,
    /// Runs without block production, follows `Leader`s blocks, and signs settlement
    /// attestations for the on-chain quorum.
    Follower,
    /// Follows `Leader`s blocks without joining the on-chain quorum.
    ///
    /// A hot standby for public RPC: it imports and serves the same chain and forwards
    /// transactions to the leader, but never signs a settlement attestation and holds no
    /// individual secp256k1 key. That keeps the leader and the quorum followers off the
    /// internet without changing the quorum.
    RpcFollower,
}

impl Role {
    /// Whether this role replicates the leader's chain instead of producing blocks.
    pub const fn follows_leader(self) -> bool {
        matches!(self, Self::Follower | Self::RpcFollower)
    }

    /// Whether this role signs settlement attestations for the on-chain quorum.
    pub const fn in_quorum(self) -> bool {
        matches!(self, Self::Leader | Self::Follower)
    }
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

impl LeadershipScheduleState {
    fn next_anchor_record(&self) -> Option<&LeadershipState> {
        let next_anchor = self
            .applied_anchor
            .map_or(u64::MAX, |applied| applied.saturating_add(1));
        self.transitions
            .range(..=next_anchor)
            .next_back()
            .map(|(_, record)| record)
    }
}

/// Activation-indexed schedule of finalized leadership transitions.
///
/// Every observed leadership transition is kept
/// until the applied Tempo checkpoint passes activation boundary. A watch notifier announces
/// changes, but consumers re-read and index by anchor.
///
/// An empty schedule represents the "uninitialized" state: no portal leader has been
/// observed at the local Tempo checkpoint and block production must stay off.
///
/// The schedule also carries the manifest's static quorum membership, so one handle answers
/// every role question: the transitions say *who leads* a given anchor, `rpc_followers` says
/// which members are outside the on-chain quorum for every anchor.
#[derive(Debug, Clone)]
pub struct LeadershipSchedule {
    inner: std::sync::Arc<std::sync::RwLock<LeadershipScheduleState>>,
    /// Members that replicate the chain without joining the on-chain quorum.
    ///
    /// Immutable: quorum membership is manifest-derived and a leadership transition only
    /// changes who leads. Promoting a standby means provisioning a secp256k1 key, registering
    /// it with `ZonePortal`, and updating the manifest — never a runtime transition.
    rpc_followers: std::sync::Arc<BTreeSet<PublicKey>>,
    changed: tokio::sync::watch::Sender<()>,
}

impl Default for LeadershipSchedule {
    fn default() -> Self {
        Self::uninitialized()
    }
}

impl LeadershipSchedule {
    /// Creates an empty (fenced, uninitialized) schedule with an all-quorum membership.
    pub fn uninitialized() -> Self {
        Self::for_membership(BTreeSet::new())
    }

    /// Creates an empty (fenced, uninitialized) schedule whose `rpc_followers` never settle.
    pub fn for_membership(rpc_followers: BTreeSet<PublicKey>) -> Self {
        let (changed, _) = tokio::sync::watch::channel(());
        Self {
            inner: std::sync::Arc::new(std::sync::RwLock::new(LeadershipScheduleState::default())),
            rpc_followers: std::sync::Arc::new(rpc_followers),
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
        self.inner
            .read()
            .expect("poisoned")
            .next_anchor_record()
            .cloned()
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
        let previous_next_anchor_record = state.next_anchor_record().cloned();
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
        let governing_record_changed =
            state.next_anchor_record() != previous_next_anchor_record.as_ref();
        drop(state);
        if governing_record_changed {
            self.changed.send_replace(());
        }
    }

    /// Subscribe to schedule-change notifications. The watch is a wakeup, not the schedule:
    /// consumers re-read via [`Self::leader_for`].
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Returns the role of `ed25519_public_key` for `tempo_anchor`, when governed.
    pub fn role_for(&self, ed25519_public_key: &PublicKey, tempo_anchor: u64) -> Option<Role> {
        let record = self.leader_for(tempo_anchor)?;
        Some(role_of(
            ed25519_public_key,
            &record.leader,
            &self.rpc_followers,
        ))
    }

    /// Whether `ed25519_public_key` replicates the chain without joining the on-chain quorum.
    pub fn is_rpc_only(&self, ed25519_public_key: &PublicKey) -> bool {
        self.rpc_followers.contains(ed25519_public_key)
    }

    /// Whether `ed25519_public_key` is registered with `ZonePortal` for settlement.
    ///
    /// Membership is static, so this holds for every anchor: it is the authority for who may
    /// be asked for a settlement signature and whose signature is accepted.
    pub fn is_quorum_member(&self, ed25519_public_key: &PublicKey) -> bool {
        !self.is_rpc_only(ed25519_public_key)
    }

    /// The subset of `peers` that belongs to the on-chain settlement quorum.
    pub fn quorum_peers(&self, peers: &[PublicKey]) -> Vec<PublicKey> {
        peers
            .iter()
            .filter(|peer| self.is_quorum_member(peer))
            .cloned()
            .collect()
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

/// Classifies `ed25519_public_key` from the two independent authorities: who leads, and which
/// members the manifest keeps out of the on-chain quorum.
///
/// Non-membership is deliberately not a fallback: a caller that cannot supply `rpc_followers`
/// would silently enrol every standby into the quorum.
fn role_of(
    ed25519_public_key: &PublicKey,
    leader: &PublicKey,
    rpc_followers: &BTreeSet<PublicKey>,
) -> Role {
    if ed25519_public_key == leader {
        Role::Leader
    } else if rpc_followers.contains(ed25519_public_key) {
        Role::RpcFollower
    } else {
        Role::Follower
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
    secp256k1_address: Option<EthereumAddress>,
    address: ManifestAddress,
    rpc_only: bool,
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
    ///
    /// `None` for an `rpc_only` node: it never signs a settlement attestation, so it holds no
    /// individual key and has nothing registered with `ZonePortal`.
    pub const fn secp256k1_address(&self) -> Option<EthereumAddress> {
        self.secp256k1_address
    }

    /// Node's advertised P2P address.
    pub const fn address(&self) -> &ManifestAddress {
        &self.address
    }

    /// Whether this node replicates the chain without joining the on-chain quorum.
    pub const fn is_rpc_only(&self) -> bool {
        self.rpc_only
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
        warn_unknown_keys("manifest", &raw.unknown);
        if raw.sequencer_set_version == 0 {
            return Err(ManifestError::InvalidSequencerSetVersion);
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
            warn_unknown_keys(&format!("nodes.{}", raw_node.name), &raw_node.unknown);
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

            // An `rpc_only` node never signs, so it must not carry an individual key: an address
            // in the manifest is either dead weight or, worse, registered with `ZonePortal` and
            // counted towards a threshold the zone can never reach.
            let secp256k1_address = match (raw_node.secp256k1_address, raw_node.rpc_only) {
                (Some(address), false) => {
                    let address = address.parse::<EthereumAddress>().map_err(|source| {
                        ManifestError::InvalidSecp256k1Address {
                            node: raw_node.name.clone(),
                            address,
                            reason: source.to_string(),
                        }
                    })?;
                    if !secp256k1_addresses.insert(address) {
                        return Err(ManifestError::DuplicateSecp256k1Address(address));
                    }
                    Some(address)
                }
                (None, true) => None,
                (None, false) => {
                    return Err(ManifestError::MissingSecp256k1Address(raw_node.name));
                }
                (Some(_), true) => {
                    return Err(ManifestError::RpcOnlySecp256k1Address(raw_node.name));
                }
            };

            let address =
                raw_node
                    .address
                    .parse()
                    .map_err(|reason| ManifestError::InvalidAddress {
                        node: raw_node.name.clone(),
                        address: raw_node.address.clone(),
                        reason,
                    })?;
            if raw_node.rpc_only && ed25519_public_key == leader_ed25519_public_key {
                return Err(ManifestError::RpcOnlyLeader(raw_node.name));
            }
            nodes.push(ManifestNode {
                name: raw_node.name,
                ed25519_public_key,
                secp256k1_address,
                address,
                rpc_only: raw_node.rpc_only,
            });
        }

        if !ed25519_public_keys.contains(&leader_ed25519_public_key) {
            return Err(ManifestError::LeaderEd25519PublicKeyNotFound(
                leader_ed25519_public_key.to_string(),
            ));
        }

        // RPC-only members are not registered with `ZonePortal`, so they cannot make up for a
        // quorum that is too small to settle.
        let quorum_node_count = nodes.iter().filter(|node| !node.rpc_only).count();
        if quorum_node_count < MIN_QUORUM_NODES {
            return Err(ManifestError::TooFewQuorumNodes(quorum_node_count));
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

    /// Validates node-specific invariants and returns the manifest-derived bootstrap role.
    ///
    /// `local_secp256k1_address` must be `None` exactly when this node is `rpc_only`: an
    /// internet-facing standby is not supposed to be provisioned with quorum key material, and
    /// a quorum member without it could never settle.
    pub fn validate_node(
        &self,
        expected_zone_id: u32,
        local_ed25519_public_key: &PublicKey,
        local_secp256k1_address: Option<EthereumAddress>,
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
        match (local_node.secp256k1_address, local_secp256k1_address) {
            (Some(manifest), Some(local)) if manifest != local => {
                return Err(ManifestError::LocalSecp256k1AddressMismatch { manifest, local });
            }
            (Some(_), None) => {
                return Err(ManifestError::LocalSecp256k1KeyMissing(
                    local_node.name.clone(),
                ));
            }
            (None, Some(_)) => {
                return Err(ManifestError::LocalSecp256k1KeyUnexpected(
                    local_node.name.clone(),
                ));
            }
            (Some(_), Some(_)) | (None, None) => {}
        }
        let role = self.bootstrap_role_of(local_ed25519_public_key);
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

    /// Role of `ed25519_public_key` under the manifest's bootstrap leader.
    pub fn bootstrap_role_of(&self, ed25519_public_key: &PublicKey) -> Role {
        role_of(
            ed25519_public_key,
            &self.leader_ed25519_public_key,
            &self.rpc_follower_keys(),
        )
    }

    /// An empty schedule carrying this manifest's static quorum membership.
    pub fn leadership_schedule(&self) -> LeadershipSchedule {
        LeadershipSchedule::for_membership(self.rpc_follower_keys())
    }

    /// All nodes in the static peer set.
    pub fn nodes(&self) -> &[ManifestNode] {
        &self.nodes
    }

    /// Nodes registered with `ZonePortal` for the on-chain settlement quorum.
    pub fn quorum_nodes(&self) -> impl Iterator<Item = &ManifestNode> {
        self.nodes.iter().filter(|node| !node.rpc_only)
    }

    /// Digest of the settlement-relevant membership, bound into the P2P network namespace.
    ///
    /// Covers each member's Ed25519 identity, its quorum standing, and the individual address
    /// its signatures must recover to — everything a peer must agree on for two nodes to derive
    /// the same roles from their own manifest copies. Peer addresses and the zone/portal identity
    /// are excluded: the latter are already namespaced separately, and relocating a node must
    /// stay a rolling operation.
    ///
    /// Iteration is over a sorted set, so the digest does not depend on entry order in the file.
    pub fn membership_digest(&self) -> B256 {
        let mut members = self
            .nodes
            .iter()
            .map(|node| {
                (
                    node.ed25519_public_key.as_ref().to_vec(),
                    node.rpc_only,
                    node.secp256k1_address,
                )
            })
            .collect::<Vec<_>>();
        members.sort();

        let mut preimage = Vec::with_capacity(members.len() * 64);
        for (ed25519_public_key, rpc_only, secp256k1_address) in members {
            preimage.extend_from_slice(&ed25519_public_key);
            preimage.push(u8::from(rpc_only));
            // Distinguish "no address" from any real address rather than substituting zero,
            // which is a valid (if useless) 20-byte value.
            match secp256k1_address {
                Some(address) => {
                    preimage.push(1);
                    preimage.extend_from_slice(address.as_slice());
                }
                None => preimage.push(0),
            }
        }
        alloy_primitives::keccak256(&preimage)
    }

    /// Ed25519 keys of the members that replicate without joining the on-chain quorum.
    pub fn rpc_follower_keys(&self) -> BTreeSet<PublicKey> {
        self.nodes
            .iter()
            .filter(|node| node.rpc_only)
            .map(|node| node.ed25519_public_key.clone())
            .collect()
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

    /// Returns the quorum node registered with an individual secp256k1 address.
    pub fn node_by_secp256k1_address(
        &self,
        secp256k1_address: EthereumAddress,
    ) -> Option<&ManifestNode> {
        self.nodes
            .iter()
            .find(|node| node.secp256k1_address() == Some(secp256k1_address))
    }

    pub(crate) fn has_dns_addresses(&self) -> bool {
        self.nodes.iter().any(|node| node.address.is_dns())
    }
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    zone_id: u32,
    #[serde(default = "default_sequencer_set_version")]
    sequencer_set_version: u64,
    leader_ed25519_public_key: String,
    nodes: Vec<RawManifestNode>,
    /// Keys this binary does not know. See [`warn_unknown_keys`].
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

const fn default_sequencer_set_version() -> u64 {
    1
}

#[derive(Debug, Deserialize)]
struct RawManifestNode {
    name: String,
    ed25519_public_key: String,
    /// Omitted exactly for `rpc_only` nodes, which never sign a settlement attestation.
    #[serde(default)]
    secp256k1_address: Option<String>,
    address: String,
    /// Serve RPC as a hot standby without joining the on-chain settlement quorum.
    #[serde(default)]
    rpc_only: bool,
    /// Keys this binary does not know. See [`warn_unknown_keys`].
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

/// Log unknown manifest keys instead of failing on them.
///
/// The manifest is shared across the fleet, so rejecting unknown keys makes every added field a
/// coordination hazard: in a rolling deployment where the manifest is updated before every binary
/// is replaced, the next restart of a node running the old binary aborts during parsing and takes
/// block production or settlement availability with it. Warning keeps a manifest that names a
/// newer field readable by an older binary, so the manifest and the binaries can be rolled
/// independently.
///
/// A warning is enough here because a mistyped key cannot quietly change this node's role: a
/// misspelled `rpc_only` leaves an entry that declares no `secp256k1_address` looking like a
/// quorum node, which [`ManifestError::MissingSecp256k1Address`] rejects, and a misspelled
/// `secp256k1_address` fails the same way.
fn warn_unknown_keys(context: &str, unknown: &BTreeMap<String, toml::Value>) {
    if unknown.is_empty() {
        return;
    }
    let keys = unknown.keys().cloned().collect::<Vec<_>>().join(", ");
    tracing::warn!(
        target: "zone::p2p",
        %context,
        %keys,
        "Ignoring sequencer manifest keys this binary does not recognize; check for a typo, or for a manifest written for a newer binary"
    );
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

    #[error(
        "sequencer manifest must contain at least {MIN_QUORUM_NODES} quorum nodes (nodes without `rpc_only`), found {0}"
    )]
    TooFewQuorumNodes(usize),

    #[error("sequencer manifest leader `{0}` cannot be `rpc_only`")]
    RpcOnlyLeader(String),

    #[error("sequencer manifest node `{0}` must declare a secp256k1_address")]
    MissingSecp256k1Address(String),

    #[error(
        "`rpc_only` sequencer manifest node `{0}` must not declare a secp256k1_address: it never signs a settlement attestation, and registering the address with `ZonePortal` would add a signer the zone never collects a signature from"
    )]
    RpcOnlySecp256k1Address(String),

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

    #[error(
        "manifest node `{0}` is a quorum member, so --secp256k1.key is required to sign settlement attestations"
    )]
    LocalSecp256k1KeyMissing(String),

    #[error(
        "manifest node `{0}` is `rpc_only`, so --secp256k1.key must not be provided: the key would never be used and must not be registered with `ZonePortal`"
    )]
    LocalSecp256k1KeyUnexpected(String),

    #[error("--sequencer.role asserts `{asserted}`, but the manifest assigns `{manifest}`")]
    RoleMismatch { asserted: Role, manifest: Role },
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::{
        LeadershipSchedule, LeadershipState, MIN_QUORUM_NODES, ManifestError, Role, ZoneManifest,
    };

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
    fn schedule_watch_announces_effective_changes() {
        let schedule = LeadershipSchedule::uninitialized();
        let mut watcher = schedule.subscribe();
        assert!(!watcher.has_changed().unwrap());
        schedule
            .publish(LeadershipState::new(1, public_key(1), 0))
            .unwrap();
        assert!(watcher.has_changed().unwrap());
        watcher.borrow_and_update();

        // Seed the applied cursor while only epoch 1 is known.
        schedule.record_applied_anchor(98);
        assert!(!watcher.has_changed().unwrap());

        schedule
            .publish(LeadershipState::new(2, public_key(2), 100))
            .unwrap();
        assert!(watcher.has_changed().unwrap());
        watcher.borrow_and_update();

        // Advancing within one leadership record (or backwards) is silent.
        schedule.record_applied_anchor(98);
        assert!(!watcher.has_changed().unwrap());
        schedule.record_applied_anchor(50);
        assert!(!watcher.has_changed().unwrap());

        // Applying anchor 99 moves the next anchor to epoch 2's activation boundary.
        schedule.record_applied_anchor(99);
        assert!(watcher.has_changed().unwrap());
        watcher.borrow_and_update();

        schedule.record_applied_anchor(150);
        assert!(!watcher.has_changed().unwrap());
    }

    fn ed25519_public_key(seed: u64) -> String {
        let key = PrivateKey::from_seed(seed).public_key();
        const_hex::encode_prefixed(key.as_ref())
    }

    fn secp256k1_address(seed: u64) -> String {
        format!("0x{seed:040x}")
    }

    fn manifest(leader: u64, nodes: &[(u64, &str, &str)]) -> String {
        let quorum = nodes
            .iter()
            .map(|(key, name, address)| (*key, *name, *address, false))
            .collect::<Vec<_>>();
        manifest_with_rpc_only(leader, &quorum)
    }

    /// Builds a manifest where the fourth tuple element marks a node `rpc_only`. An `rpc_only`
    /// node declares no `secp256k1_address`, exactly as the loader requires.
    fn manifest_with_rpc_only(leader: u64, nodes: &[(u64, &str, &str, bool)]) -> String {
        let mut value = format!(
            "zone_id = 7\nleader_ed25519_public_key = \"{}\"\n",
            ed25519_public_key(leader)
        );
        for (key, name, address, rpc_only) in nodes {
            value.push_str(&format!(
                "\n[[nodes]]\nname = \"{name}\"\ned25519_public_key = \"{}\"\naddress = \"{address}\"\nrpc_only = {rpc_only}\n",
                ed25519_public_key(*key),
            ));
            if !rpc_only {
                value.push_str(&format!(
                    "secp256k1_address = \"{}\"\n",
                    secp256k1_address(*key)
                ));
            }
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
        let schedule = manifest.leadership_schedule();
        schedule.publish(leadership).unwrap();
        assert_eq!(schedule.role_for(&leader, 0), Some(Role::Leader));
        assert_eq!(schedule.role_for(&follower, 0), Some(Role::Follower));
        assert_eq!(manifest.bootstrap_role_of(&leader), Role::Leader);
        assert_eq!(manifest.bootstrap_role_of(&follower), Role::Follower);

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
                .validate_node(
                    7,
                    &leader,
                    Some(secp256k1_address(1).parse().unwrap()),
                    None
                )
                .unwrap(),
            Role::Leader
        );
        assert_eq!(
            manifest
                .validate_node(
                    7,
                    &follower,
                    Some(secp256k1_address(2).parse().unwrap()),
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

        // The example uses placeholder keys, so only its shape can be checked.
        let manifest: super::RawManifest = toml::from_str(example).unwrap();
        assert_eq!(manifest.zone_id, 7);
        assert_eq!(manifest.nodes.len(), 4);
        assert_eq!(
            manifest.nodes.iter().filter(|node| !node.rpc_only).count(),
            MIN_QUORUM_NODES
        );
        // An `rpc_only` entry carries no individual key.
        assert!(
            manifest
                .nodes
                .iter()
                .all(|node| node.rpc_only != node.secp256k1_address.is_some())
        );
    }

    #[test]
    fn rpc_only_nodes_replicate_without_joining_the_quorum() {
        let input = manifest_with_rpc_only(
            1,
            &[
                (1, "leader", "127.0.0.1:9200", false),
                (2, "follower-a", "127.0.0.1:9201", false),
                (3, "follower-b", "127.0.0.1:9202", false),
                (4, "public-rpc", "127.0.0.1:9203", true),
            ],
        );
        let manifest = ZoneManifest::parse(&input).unwrap();
        let leader = public_key(1);
        let follower = public_key(2);
        let rpc_follower = public_key(4);

        assert_eq!(manifest.bootstrap_role_of(&leader), Role::Leader);
        assert_eq!(manifest.bootstrap_role_of(&follower), Role::Follower);
        assert_eq!(manifest.bootstrap_role_of(&rpc_follower), Role::RpcFollower);
        assert!(Role::RpcFollower.follows_leader());
        assert!(!Role::RpcFollower.in_quorum());

        // The standby replicates, but the on-chain quorum is unchanged by its presence, and it
        // carries no address that could be registered with `ZonePortal`.
        assert_eq!(manifest.nodes().len(), 4);
        assert_eq!(manifest.quorum_nodes().count(), MIN_QUORUM_NODES);
        assert!(
            manifest
                .quorum_nodes()
                .all(|node| node.ed25519_public_key() != &rpc_follower)
        );
        let standby = manifest
            .node_by_ed25519_public_key(&rpc_follower)
            .expect("the standby is a manifest member");
        assert!(standby.is_rpc_only());
        assert_eq!(standby.secp256k1_address(), None);

        // The schedule carries the membership, so classification survives a leader rotation.
        let schedule = manifest.leadership_schedule();
        assert!(schedule.is_rpc_only(&rpc_follower));
        assert!(!schedule.is_quorum_member(&rpc_follower));
        assert!(schedule.is_quorum_member(&follower));
        assert_eq!(
            schedule.quorum_peers(&[leader.clone(), follower.clone(), rpc_follower.clone()]),
            vec![leader, follower.clone()]
        );
        schedule
            .publish(LeadershipState::new(1, follower.clone(), 0))
            .unwrap();
        assert_eq!(schedule.role_for(&follower, 0), Some(Role::Leader));
        assert_eq!(schedule.role_for(&rpc_follower, 0), Some(Role::RpcFollower));

        // Its role assertion must name the standby role, not `follower`.
        assert_eq!(
            manifest
                .validate_node(7, &rpc_follower, None, Some(Role::RpcFollower))
                .unwrap(),
            Role::RpcFollower
        );
        assert!(matches!(
            manifest.validate_node(7, &rpc_follower, None, Some(Role::Follower)),
            Err(ManifestError::RoleMismatch { .. })
        ));
    }

    #[test]
    fn rejects_topologies_that_would_shrink_or_miskey_the_quorum() {
        // Four nodes, but only two of them can sign a settlement.
        let thin_quorum = manifest_with_rpc_only(
            1,
            &[
                (1, "leader", "127.0.0.1:9200", false),
                (2, "follower", "127.0.0.1:9201", false),
                (3, "public-rpc-a", "127.0.0.1:9202", true),
                (4, "public-rpc-b", "127.0.0.1:9203", true),
            ],
        );
        assert!(matches!(
            ZoneManifest::parse(&thin_quorum),
            Err(ManifestError::TooFewQuorumNodes(2))
        ));

        // A leader cannot opt out of the quorum it settles for.
        let rpc_only_leader = manifest_with_rpc_only(
            1,
            &[
                (1, "leader", "127.0.0.1:9200", true),
                (2, "follower-a", "127.0.0.1:9201", false),
                (3, "follower-b", "127.0.0.1:9202", false),
            ],
        );
        assert!(matches!(
            ZoneManifest::parse(&rpc_only_leader),
            Err(ManifestError::RpcOnlyLeader(_))
        ));

        let quorum_nodes = [
            (1, "leader", "127.0.0.1:9200", false),
            (2, "follower-a", "127.0.0.1:9201", false),
            (3, "follower-b", "127.0.0.1:9202", false),
        ];

        // A quorum node without an individual address could never settle.
        let missing_address = manifest_with_rpc_only(1, &quorum_nodes).replace(
            &format!("secp256k1_address = \"{}\"\n", secp256k1_address(3)),
            "",
        );
        assert!(matches!(
            ZoneManifest::parse(&missing_address),
            Err(ManifestError::MissingSecp256k1Address(node)) if node == "follower-b"
        ));

        // An address on a standby is at best dead weight and at worst a registered signer the
        // zone never collects a signature from.
        let mut standby_with_address = manifest_with_rpc_only(
            1,
            &[
                (1, "leader", "127.0.0.1:9200", false),
                (2, "follower-a", "127.0.0.1:9201", false),
                (3, "follower-b", "127.0.0.1:9202", false),
                (4, "public-rpc", "127.0.0.1:9203", true),
            ],
        );
        standby_with_address.push_str(&format!(
            "secp256k1_address = \"{}\"\n",
            secp256k1_address(4)
        ));
        assert!(matches!(
            ZoneManifest::parse(&standby_with_address),
            Err(ManifestError::RpcOnlySecp256k1Address(node)) if node == "public-rpc"
        ));
    }

    #[test]
    fn membership_digest_tracks_quorum_standing_but_not_addresses_or_order() {
        let nodes = [
            (1, "leader", "127.0.0.1:9200", false),
            (2, "follower-a", "127.0.0.1:9201", false),
            (3, "follower-b", "127.0.0.1:9202", false),
            (4, "public-rpc", "127.0.0.1:9203", true),
        ];
        let baseline = ZoneManifest::parse(&manifest_with_rpc_only(1, &nodes))
            .unwrap()
            .membership_digest();

        // Entry order in the file is not part of the identity.
        let mut reordered = nodes;
        reordered.swap(0, 3);
        assert_eq!(
            ZoneManifest::parse(&manifest_with_rpc_only(1, &reordered))
                .unwrap()
                .membership_digest(),
            baseline
        );

        // Relocating a node must stay a rolling operation, so its address is excluded.
        let moved = [
            (1, "leader", "127.0.0.1:9200", false),
            (2, "follower-a", "follower-a.zone.local:9300", false),
            (3, "follower-b", "127.0.0.1:9202", false),
            (4, "public-rpc", "127.0.0.1:9203", true),
        ];
        assert_eq!(
            ZoneManifest::parse(&manifest_with_rpc_only(1, &moved))
                .unwrap()
                .membership_digest(),
            baseline
        );

        // Moving a member in or out of the quorum is a different network.
        let promoted = [
            (1, "leader", "127.0.0.1:9200", false),
            (2, "follower-a", "127.0.0.1:9201", false),
            (3, "follower-b", "127.0.0.1:9202", false),
            (4, "public-rpc", "127.0.0.1:9203", false),
        ];
        assert_ne!(
            ZoneManifest::parse(&manifest_with_rpc_only(1, &promoted))
                .unwrap()
                .membership_digest(),
            baseline
        );

        // ...and so is changing the address a member's signatures must recover to.
        let rekeyed = manifest_with_rpc_only(1, &nodes).replace(
            &format!("secp256k1_address = \"{}\"", secp256k1_address(3)),
            &format!("secp256k1_address = \"{}\"", secp256k1_address(9)),
        );
        assert_ne!(
            ZoneManifest::parse(&rekeyed).unwrap().membership_digest(),
            baseline
        );
    }

    #[test]
    fn unknown_keys_are_tolerated_but_cannot_silently_change_a_role() {
        let quorum = [
            (1, "leader", "127.0.0.1:9200", false),
            (2, "follower-a", "127.0.0.1:9201", false),
            (3, "follower-b", "127.0.0.1:9202", false),
        ];

        // A manifest written for a newer binary must stay readable by this one, so the manifest
        // and the binaries can be rolled independently.
        let mut from_the_future = manifest_with_rpc_only(1, &quorum);
        from_the_future.push_str("some_future_node_field = \"value\"\n");
        from_the_future.insert_str(0, "some_future_top_level_field = 42\n");
        let manifest = ZoneManifest::parse(&from_the_future).unwrap();
        assert_eq!(manifest.quorum_nodes().count(), MIN_QUORUM_NODES);

        // A misspelled `rpc_only` leaves an entry with no `secp256k1_address` looking like a
        // quorum node, so tolerating unknown keys cannot quietly enrol a standby in the quorum.
        let typo = manifest_with_rpc_only(
            1,
            &[
                (1, "leader", "127.0.0.1:9200", false),
                (2, "follower-a", "127.0.0.1:9201", false),
                (3, "follower-b", "127.0.0.1:9202", false),
                (4, "public-rpc", "127.0.0.1:9203", true),
            ],
        )
        .replace("rpc_only = true", "rpc-only = true");
        assert!(matches!(
            ZoneManifest::parse(&typo),
            Err(ManifestError::MissingSecp256k1Address(node)) if node == "public-rpc"
        ));
    }

    #[test]
    fn role_round_trips_through_its_cli_and_manifest_spelling() {
        for role in [Role::Leader, Role::Follower, Role::RpcFollower] {
            assert_eq!(role.to_string().parse::<Role>().unwrap(), role);
        }
        assert_eq!("rpc-follower".parse::<Role>().unwrap(), Role::RpcFollower);
        assert!("rpc_follower".parse::<Role>().is_err());
    }

    #[test]
    fn local_key_presence_must_match_the_manifest_entry() {
        let manifest = ZoneManifest::parse(&manifest_with_rpc_only(
            1,
            &[
                (1, "leader", "127.0.0.1:9200", false),
                (2, "follower-a", "127.0.0.1:9201", false),
                (3, "follower-b", "127.0.0.1:9202", false),
                (4, "public-rpc", "127.0.0.1:9203", true),
            ],
        ))
        .unwrap();

        // A quorum member started without --secp256k1.key cannot sign.
        assert!(matches!(
            manifest.validate_node(7, &public_key(2), None, None),
            Err(ManifestError::LocalSecp256k1KeyMissing(node)) if node == "follower-a"
        ));
        // A standby started with one holds key material it must not have.
        assert!(matches!(
            manifest.validate_node(
                7,
                &public_key(4),
                Some(secp256k1_address(4).parse().unwrap()),
                None
            ),
            Err(ManifestError::LocalSecp256k1KeyUnexpected(node)) if node == "public-rpc"
        ));
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
            Err(ManifestError::TooFewQuorumNodes(2))
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
                Some(secp256k1_address(2).parse().unwrap()),
                Some(Role::Leader),
            ),
            Err(ManifestError::RoleMismatch { .. })
        ));
        assert!(matches!(
            valid.validate_node(
                8,
                &follower,
                Some(secp256k1_address(2).parse().unwrap()),
                None,
            ),
            Err(ManifestError::ZoneIdMismatch { .. })
        ));
        let unknown = PrivateKey::from_seed(99).public_key();
        assert!(matches!(
            valid.validate_node(
                7,
                &unknown,
                Some(secp256k1_address(99).parse().unwrap()),
                None,
            ),
            Err(ManifestError::LocalNodeNotFound(_))
        ));

        assert!(matches!(
            valid.validate_node(
                7,
                &follower,
                Some(secp256k1_address(3).parse().unwrap()),
                None,
            ),
            Err(ManifestError::LocalSecp256k1AddressMismatch { .. })
        ));
    }
}
