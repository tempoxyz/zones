use std::net::SocketAddr;

use alloy_primitives::Address as EthereumAddress;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_p2p::{Address, authenticated::lookup};
use commonware_runtime::{Metrics as _, Quota};
use commonware_utils::{NZU32, ordered::Map};
use eyre::WrapErr as _;

use crate::ZoneManifest;

/// Leader-to-follower sealed block replication channel.
pub(crate) const BLOCK_CHANNEL: u64 = 0;
pub(crate) const BLOCK_BACKLOG: usize = 128;

// At 30M gas, calldata is bounded below 7.5 MiB; leave headroom for block overhead.
pub(crate) const MAX_MESSAGE_SIZE: u32 = 20 * 1024 * 1024;

/// Version of the Tempo Zone P2P wire protocol.
pub(crate) const WIRE_PROTOCOL_VERSION: u8 = 0;
const NETWORK_NAMESPACE_PREFIX: &[u8] = b"TEMPO_ZONE_P2P";

/// Immutable L1 identity used to keep P2P networks for different deployments separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct P2pNetworkId {
    l1_chain_id: u64,
    portal_address: EthereumAddress,
}

impl P2pNetworkId {
    /// Creates the P2P identity for one ZonePortal deployment.
    pub const fn new(l1_chain_id: u64, portal_address: EthereumAddress) -> Self {
        Self {
            l1_chain_id,
            portal_address,
        }
    }
}

pub(crate) type Network = lookup::Network<commonware_runtime::tokio::Context, PrivateKey>;
pub(crate) type Oracle = lookup::Oracle<PublicKey>;

pub(crate) fn instantiate(
    context: commonware_runtime::tokio::Context,
    manifest: &ZoneManifest,
    ed25519_private_key: PrivateKey,
    listen: SocketAddr,
    bypass_ip_check: bool,
    network_id: P2pNetworkId,
) -> eyre::Result<(Network, Oracle, Map<PublicKey, Address>)> {
    let namespace = namespace(manifest.zone_id(), network_id);
    let mut config = if listen.ip().is_loopback() {
        lookup::Config::local(ed25519_private_key, &namespace, listen, MAX_MESSAGE_SIZE)
    } else {
        lookup::Config::recommended(ed25519_private_key, &namespace, listen, MAX_MESSAGE_SIZE)
    };

    // Zone P2P peers commonly use pod/private addresses. Membership remains
    // authenticated by the Ed25519 identities in the manifest.
    config.allow_private_ips = true;
    // This is a network-wide Commonware setting, so it must be an explicit
    // operator choice.
    config.bypass_ip_check = bypass_ip_check;

    let peers = manifest
        .nodes()
        .iter()
        .map(|node| {
            (
                node.ed25519_public_key().clone(),
                node.address().to_commonware(),
            )
        })
        .collect::<Vec<_>>();
    let peers = Map::try_from(peers).wrap_err("manifest contains duplicate P2P identities")?;
    let (network, oracle) = lookup::Network::new(context.with_label("network"), config);
    Ok((network, oracle, peers))
}

fn namespace(zone_id: u32, network_id: P2pNetworkId) -> Vec<u8> {
    // Include both the protocol version and immutable L1 deployment identity so keys or
    // endpoints accidentally reused across local, test, and production environments cannot
    // authenticate into one another's P2P network.
    let mut namespace = Vec::with_capacity(
        NETWORK_NAMESPACE_PREFIX.len() + 1 + 8 + EthereumAddress::len_bytes() + 4,
    );
    namespace.extend_from_slice(NETWORK_NAMESPACE_PREFIX);
    namespace.push(WIRE_PROTOCOL_VERSION);
    namespace.extend_from_slice(&network_id.l1_chain_id.to_be_bytes());
    namespace.extend_from_slice(network_id.portal_address.as_slice());
    namespace.extend_from_slice(&zone_id.to_be_bytes());
    namespace
}

pub(crate) fn block_quota() -> Quota {
    Quota::per_second(NZU32!(128))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::{NETWORK_NAMESPACE_PREFIX, P2pNetworkId, WIRE_PROTOCOL_VERSION, namespace};

    #[test]
    fn namespace_separates_l1_environments_and_portals() {
        let portal_a = address!("1111111111111111111111111111111111111111");
        let portal_b = address!("2222222222222222222222222222222222222222");

        assert_ne!(
            namespace(7, P2pNetworkId::new(1, portal_a)),
            namespace(7, P2pNetworkId::new(2, portal_a)),
        );
        assert_ne!(
            namespace(7, P2pNetworkId::new(1, portal_a)),
            namespace(7, P2pNetworkId::new(1, portal_b)),
        );

        let namespace = namespace(7, P2pNetworkId::new(1, portal_a));
        assert_eq!(
            namespace[NETWORK_NAMESPACE_PREFIX.len()],
            WIRE_PROTOCOL_VERSION
        );
    }
}
