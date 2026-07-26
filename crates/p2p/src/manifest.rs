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

/// Shared, updatable leadership record for a zone node.
///
/// Owns the write side of the record so that whichever component decides leadership — an
/// admin RPC, or an L1 leadership registry watcher — can publish a new one, while every
/// role-dependent task observes it through [`Self::subscribe`].
///
/// The handle is reference-counted and every observer holds a clone, so the underlying
/// channel is alive for as long as anyone is watching it. Observers can therefore treat
/// "the channel closed" as impossible rather than as a fatal condition.
///
/// Nothing publishes a new record yet: the value stays at the manifest's bootstrap record
/// for the lifetime of the process, which keeps the static-leader behaviour unchanged.
#[derive(Debug, Clone)]
pub struct Leadership(std::sync::Arc<tokio::sync::watch::Sender<LeadershipState>>);

impl Leadership {
    /// Creates a handle seeded with `initial`.
    pub fn new(initial: LeadershipState) -> Self {
        Self(std::sync::Arc::new(tokio::sync::watch::channel(initial).0))
    }

    /// Observes the current and all future leadership records.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<LeadershipState> {
        self.0.subscribe()
    }

    /// Returns the record in force right now.
    pub fn current(&self) -> LeadershipState {
        self.0.borrow().clone()
    }

    /// Returns the role of `ed25519_public_key` under the current record.
    pub fn role_of(&self, ed25519_public_key: &PublicKey) -> Role {
        self.0.borrow().role_of(ed25519_public_key)
    }

    /// Publishes a new leadership record to every observer.
    ///
    /// Callers are responsible for the epoch ordering rules; this is a plain overwrite.
    pub fn publish(&self, state: LeadershipState) {
        self.0.send_replace(state);
    }
}

/// The leadership record currently observed by a zone node.
///
/// Epoch zero is bootstrapped from the manifest. Later leadership changes replace this
/// record through the [`Leadership`] handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadershipState {
    /// Monotonically increasing fencing epoch.
    pub epoch: u64,
    /// Ed25519 identity of the selected leader.
    pub leader: PublicKey,
    /// First block where the leader can become active.
    pub start_block: u64,
}

impl LeadershipState {
    /// Creates a leadership record.
    pub const fn new(epoch: u64, leader: PublicKey, start_block: u64) -> Self {
        Self {
            epoch,
            leader,
            start_block,
        }
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

    /// Ed25519 Commonware public key of the statically assigned leader.
    pub const fn leader_ed25519_public_key(&self) -> &PublicKey {
        &self.leader_ed25519_public_key
    }

    /// Initial leadership record used until a persisted or externally decided
    /// record is available.
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

    use super::{ManifestError, Role, ZoneManifest};

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
