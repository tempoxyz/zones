use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
};

use alloy_primitives::Address as EthereumAddress;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{Address, Ingress};
use commonware_utils::Hostname;
use serde::Deserialize;

/// Minimum number of nodes that must be registered for the on-chain settlement quorum.
const MIN_QUORUM_NODES: usize = 3;

/// The role assigned to a node by the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Builds blocks and runs the existing sequencer settlement tasks.
    Leader,
    /// Runs without block production, follows `Leader`s blocks, and signs settlement
    /// attestations for the on-chain quorum.
    Follower,
    /// Follows `Leader`s blocks without joining the on-chain quorum.
    ///
    /// A hot standby for public RPC: it imports and serves the same chain and forwards
    /// transactions to the leader, but never signs a settlement attestation. That keeps the
    /// leader and the quorum followers off the internet without changing the quorum.
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

    const fn as_str(self) -> &'static str {
        match self {
            Self::Leader => "leader",
            Self::Follower => "follower",
            Self::RpcFollower => "rpc-follower",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "leader" => Ok(Self::Leader),
            "follower" => Ok(Self::Follower),
            "rpc-follower" => Ok(Self::RpcFollower),
            other => Err(format!(
                "expected `leader`, `follower`, or `rpc-follower`, got `{other}`"
            )),
        }
    }
}

/// Shared bootstrap leadership record for a zone node.
///
/// Leadership remains immutable for the lifetime of the process. A future handoff implementation
/// will replace this read-only handle with an activation-boundary-aware schedule and expose
/// mutation only together with complete role-specific worker switching.
#[derive(Debug, Clone)]
pub struct Leadership(std::sync::Arc<LeadershipState>);

impl Leadership {
    /// Creates an immutable handle seeded from the validated manifest.
    pub(crate) fn new(initial: LeadershipState) -> Self {
        Self(std::sync::Arc::new(initial))
    }

    /// Returns the record in force right now.
    pub fn current(&self) -> LeadershipState {
        (*self.0).clone()
    }

    /// Returns the role of `ed25519_public_key` under the current record.
    pub fn role_of(&self, ed25519_public_key: &PublicKey) -> Role {
        self.0.role_of(ed25519_public_key)
    }
}

/// The leadership record bootstrapped by a zone node.
///
/// Epoch zero is derived from the manifest. Later epochs are reserved for the handoff protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadershipState {
    /// Monotonically increasing fencing epoch.
    pub epoch: u64,
    /// Ed25519 identity of the selected leader.
    pub leader: PublicKey,
    /// First block where the leader can become active.
    pub start_block: u64,
    /// Members that replicate the chain without joining the on-chain quorum.
    ///
    /// Shared rather than copied: routing consults this on every P2P command.
    rpc_followers: Arc<BTreeSet<PublicKey>>,
}

impl LeadershipState {
    /// Creates a leadership record whose members all belong to the quorum.
    pub fn new(epoch: u64, leader: PublicKey, start_block: u64) -> Self {
        Self {
            epoch,
            leader,
            start_block,
            rpc_followers: Arc::default(),
        }
    }

    /// Marks the members that replicate the chain without joining the on-chain quorum.
    pub fn with_rpc_followers(mut self, rpc_followers: BTreeSet<PublicKey>) -> Self {
        self.rpc_followers = Arc::new(rpc_followers);
        self
    }

    /// Monotonically increasing fencing epoch.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// First block governed by this record.
    pub const fn start_block(&self) -> u64 {
        self.start_block
    }

    /// Leader selected by this record.
    pub const fn leader(&self) -> &PublicKey {
        &self.leader
    }

    /// Returns the leader for `block_number` when this record governs it.
    pub fn leader_for(&self, block_number: u64) -> Option<&PublicKey> {
        (block_number >= self.start_block).then_some(&self.leader)
    }

    /// Returns the role of `ed25519_public_key` under this record.
    pub fn role_of(&self, ed25519_public_key: &PublicKey) -> Role {
        if ed25519_public_key == &self.leader {
            Role::Leader
        } else if self.rpc_followers.contains(ed25519_public_key) {
            Role::RpcFollower
        } else {
            Role::Follower
        }
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
    pub const fn secp256k1_address(&self) -> EthereumAddress {
        self.secp256k1_address
    }

    /// Node's advertised P2P address.
    pub const fn address(&self) -> &ManifestAddress {
        &self.address
    }

    /// Whether this node serves RPC without joining the on-chain settlement quorum.
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
        let quorum_nodes = nodes.iter().filter(|node| !node.rpc_only).count();
        if quorum_nodes < MIN_QUORUM_NODES {
            return Err(ManifestError::TooFewQuorumNodes(quorum_nodes));
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

    /// Ed25519 Commonware public key of the statically assigned leader.
    pub const fn leader_ed25519_public_key(&self) -> &PublicKey {
        &self.leader_ed25519_public_key
    }

    /// Static leadership record used for the lifetime of the process.
    pub fn bootstrap_leadership(&self) -> LeadershipState {
        LeadershipState::new(0, self.leader_ed25519_public_key.clone(), 0).with_rpc_followers(
            self.nodes
                .iter()
                .filter(|node| node.rpc_only)
                .map(|node| node.ed25519_public_key.clone())
                .collect(),
        )
    }

    /// All nodes in the static peer set.
    pub fn nodes(&self) -> &[ManifestNode] {
        &self.nodes
    }

    /// Nodes registered with `ZonePortal` for the on-chain settlement quorum.
    pub fn quorum_nodes(&self) -> impl Iterator<Item = &ManifestNode> {
        self.nodes.iter().filter(|node| !node.rpc_only)
    }

    /// Returns whether an Ed25519 key belongs to the manifest's static peer set.
    pub fn contains_ed25519_public_key(&self, ed25519_public_key: &PublicKey) -> bool {
        self.node_by_ed25519_public_key(ed25519_public_key)
            .is_some()
    }

    fn node_by_ed25519_public_key(&self, ed25519_public_key: &PublicKey) -> Option<&ManifestNode> {
        self.nodes
            .iter()
            .find(|node| node.ed25519_public_key() == ed25519_public_key)
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
    /// Serve RPC as a hot standby without joining the on-chain settlement quorum.
    #[serde(default)]
    rpc_only: bool,
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

    use super::{MIN_QUORUM_NODES, ManifestError, Role, ZoneManifest};

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

    fn manifest_with_rpc_only(leader: u64, nodes: &[(u64, &str, &str, bool)]) -> String {
        let mut value = format!(
            "zone_id = 7\nleader_ed25519_public_key = \"{}\"\n",
            ed25519_public_key(leader)
        );
        for (key, name, address, rpc_only) in nodes {
            value.push_str(&format!(
                "\n[[nodes]]\nname = \"{name}\"\ned25519_public_key = \"{}\"\nsecp256k1_address = \"{}\"\naddress = \"{address}\"\nrpc_only = {rpc_only}\n",
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
        assert_eq!(leadership.start_block(), 0);
        assert_eq!(leadership.leader_for(0), Some(&leader));
        assert_eq!(leadership.leader_for(u64::MAX), Some(&leader));
        assert_eq!(leadership.role_of(&leader), Role::Leader);
        assert_eq!(leadership.role_of(&follower), Role::Follower);
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

        // The example uses placeholder keys, so only its shape can be checked.
        let manifest: super::RawManifest = toml::from_str(example).unwrap();
        assert_eq!(manifest.zone_id, 7);
        assert_eq!(manifest.nodes.len(), 4);
        assert_eq!(
            manifest.nodes.iter().filter(|node| !node.rpc_only).count(),
            MIN_QUORUM_NODES
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
        let leader = PrivateKey::from_seed(1).public_key();
        let follower = PrivateKey::from_seed(2).public_key();
        let rpc_follower = PrivateKey::from_seed(4).public_key();
        let leadership = manifest.bootstrap_leadership();

        assert_eq!(leadership.role_of(&leader), Role::Leader);
        assert_eq!(leadership.role_of(&follower), Role::Follower);
        assert_eq!(leadership.role_of(&rpc_follower), Role::RpcFollower);

        // The RPC standby replicates, but the on-chain quorum is unchanged by its presence.
        assert!(Role::RpcFollower.follows_leader());
        assert!(!Role::RpcFollower.in_quorum());
        assert_eq!(manifest.nodes().len(), 4);
        assert_eq!(manifest.quorum_nodes().count(), 3);
        assert!(
            manifest
                .quorum_nodes()
                .all(|node| node.ed25519_public_key() != &rpc_follower)
        );

        assert_eq!(
            manifest
                .validate_node(
                    7,
                    &rpc_follower,
                    secp256k1_address(4).parse().unwrap(),
                    Some(Role::RpcFollower),
                )
                .unwrap(),
            Role::RpcFollower
        );
        assert!(matches!(
            manifest.validate_node(
                7,
                &rpc_follower,
                secp256k1_address(4).parse().unwrap(),
                Some(Role::Follower),
            ),
            Err(ManifestError::RoleMismatch { .. })
        ));
    }

    #[test]
    fn rejects_topologies_that_would_shrink_the_quorum() {
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
