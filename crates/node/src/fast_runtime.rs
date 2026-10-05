//! Finalized T14 capability import and production OpenRaft assembly.

#![allow(clippy::result_large_err)] // Startup errors preserve complete OpenRaft diagnostics.

use std::{
    collections::{BTreeMap, HashMap},
    fs::OpenOptions,
    future::Future,
    io::Write as _,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use alloy_consensus::{BlockHeader as _, Sealable as _, Transaction as _};
use alloy_contract::CallBuilder;
use alloy_eips::{BlockNumberOrTag, NumHash};
use alloy_primitives::{Address, B256, b256, keccak256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolValue as _;
use openraft::{BasicNode, Config};
use rand::{RngCore as _, rngs::OsRng};
use reth_storage_api::{BlockNumReader, BlockReader};
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::ZonePortal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zone_chainspec::ZoneChainSpec;
use zone_p2p::{P2pCommand, P2pPeerId, RaftPorts, RaftRequestFrame};

use crate::{
    engine::{FastActivationRefresh, FastActivationRefreshFuture, FastAuthorityRefresh},
    fast_execution::CanonicalFastExecution,
    fast_network::{
        AuthenticatedRaftTransport, FastNetworkConfig, FastRaftPeerHandler, FinalizedPeerIdentity,
        HandlerFuture, SignedOutcome,
    },
    fast_quorum::{
        AuthenticatedRaftPeer, FastActivation, FastRaftRuntime, FinalizedFastEpoch,
        FinalizedT14Capability, RaftCommit, assemble_fast_raft, verify_committed_certificate,
    },
    fast_raft_state_machine::{
        CommittedStateHandle, CommittedTransferRecord, DurableStateMachineExecution,
    },
    fast_service::{
        CommittedTransferSource, FastServiceConfig, FastServiceError, FastServiceRoute,
        PeerEndpoint, ServiceFuture,
    },
};
use zone_evm::same_anchor::SameAnchorOpening;
use zone_fast_transfer::{
    DurableJournal, EpochRoster, InventoryContribution, ProtocolLimits, QuorumVerifier,
    ReplenishmentJob, ReplenishmentWorker,
    admission::{RouteKey, ValueCaps},
};
use zone_primitives::fast_transfer::{
    CanonicalEncode as _, CertificateBody, MAX_CERTIFICATE_BYTES, OutcomeCertificate,
    SignatureBytes, ZoneDomain, decode_exact,
};
use zone_sequencer::fast_replenishment::{
    AlloyReplenishmentProviderHandles, FilePreparedActionStore, FileReplenishmentNoncePlanner,
    ProviderBackedReplenishmentBridge, ReplenishmentProviderError, ReplenishmentRouteConfig,
};

pub type CertificationFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

const ENROLLMENT_SENTINEL: &str = "fast-epoch-enrollment-v1";
const ENROLLMENT_SENTINEL_DOMAIN: &[u8] = b"tempo.zone.fast-epoch-enrollment.v1";

/// Completion means every locally reconstructed outcome for an entry has a verified, fsynced
/// two-member certificate available to committed RPC reads.
pub trait FastOutcomeCertification: Send + Sync + 'static {
    fn certify_commit(&self, commit: RaftCommit) -> CertificationFuture<'_>;
}

pub struct LoadedFastActivation {
    pub activation: FastActivation,
    pub admission_open: bool,
    pub proof_policy: FinalizedFastProofPolicy,
    /// Initialization is authorized only at the exact finalized L1 block that activates the
    /// epoch. A later restart with missing Raft state is recovery and must fail closed.
    pub allow_initialize: bool,
    pub anchor_timestamp: u64,
    pub anchor_timestamp_millis_part: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalizedFastProofPolicy {
    pub mode: u8,
    pub expected_verifier_code_hash: B256,
    pub expected_verifier_config_hash: B256,
}

const FAST_EPOCH_CONFIG_WORDS: usize = 27;
const PROOF_MODE_OPERATOR_ATTESTED: u8 = 1;
const PROOF_MODE_REQUIRED: u8 = 2;
const DEVELOPMENT_PROTOTYPE_VERIFIER_CODE_HASH: B256 =
    b256!("c6bc17dc6724fb475ce3c59ec94e01bd733c2996b41bc82281b4d638b928cc33");

#[derive(Clone, Copy, Debug)]
struct FinalizedPortalEpochConfig {
    protocol_version: u32,
    threshold: u8,
    proof_mode: u8,
    closed: bool,
    retired: bool,
    activated_at_tempo_block: u64,
    roster_hash: B256,
    peers_hash: B256,
    expected_verifier_code_hash: B256,
    expected_verifier_config_hash: B256,
}

/// Exact-anchor lifecycle validator installed into the production engine. A changed roster/epoch
/// requires a restart and fresh assembly; this seam only preserves or revokes current authority.
pub struct ExactAnchorActivationRefresh {
    l1: DynProvider<TempoNetwork>,
    portal_address: Address,
    chain_spec: Arc<ZoneChainSpec>,
    zone_id: u32,
    zone_chain_id: u64,
    expected: FastActivation,
    expected_proof_policy: FinalizedFastProofPolicy,
}

impl ExactAnchorActivationRefresh {
    pub fn new(
        l1: DynProvider<TempoNetwork>,
        portal_address: Address,
        chain_spec: Arc<ZoneChainSpec>,
        zone_id: u32,
        zone_chain_id: u64,
        expected: FastActivation,
        expected_proof_policy: FinalizedFastProofPolicy,
    ) -> Self {
        Self {
            l1,
            portal_address,
            chain_spec,
            zone_id,
            zone_chain_id,
            expected,
            expected_proof_policy,
        }
    }
}

impl FastActivationRefresh for ExactAnchorActivationRefresh {
    fn refresh(&self, anchor: NumHash) -> FastActivationRefreshFuture<'_> {
        Box::pin(async move {
            let loaded = load_fast_activation(
                &self.l1,
                self.portal_address,
                &self.chain_spec,
                anchor,
                self.zone_id,
                self.zone_chain_id,
            )
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "T14 fast capability is inactive at the imported anchor".to_owned())?;
            if loaded.activation.epoch() != self.expected.epoch() {
                return Err(
                    "finalized fast epoch or roster changed; restart is required".to_owned(),
                );
            }
            if loaded.proof_policy != self.expected_proof_policy {
                return Err("finalized fast proof policy changed; restart is required".to_owned());
            }
            let opening = SameAnchorOpening::v1(
                anchor.number,
                anchor.hash,
                loaded.anchor_timestamp,
                loaded.anchor_timestamp_millis_part,
                loaded.activation.epoch().epoch,
            );
            Ok(if loaded.admission_open {
                FastAuthorityRefresh::Open(opening)
            } else {
                FastAuthorityRefresh::Drain(opening)
            })
        })
    }
}

/// Operator routing and local durable resources. None of these fields activates the protocol;
/// activation is derived exclusively from finalized Portal state by [`load_fast_activation`].
#[derive(Clone, Debug)]
pub struct FastRuntimeConfig {
    pub signer: PrivateKeySigner,
    pub local_transport: P2pPeerId,
    pub member_transports: BTreeMap<Address, P2pPeerId>,
    pub storage: PathBuf,
    pub rpc_timeout: Duration,
    pub service: Option<FastServiceRuntimeConfig>,
    pub replenishment_routes: Vec<FastReplenishmentRuntimeConfig>,
}

impl FastRuntimeConfig {
    pub fn validate(&self) -> Result<(), FastRuntimeError> {
        if self.member_transports.len() != 3
            || self.member_transports.contains_key(&Address::ZERO)
            || self.rpc_timeout.is_zero()
            || self.service.is_none()
            || self
                .service
                .as_ref()
                .is_some_and(|service| !service.is_valid())
            || self.replenishment_routes.iter().any(|route| {
                route.poll_interval.is_zero()
                    || route.storage.as_os_str().is_empty()
                    || route.expected_source_fast_epoch == 0
                    || route.expected_destination_fast_epoch == 0
                    || route.expected_destination_key_x.is_zero()
                    || !matches!(route.expected_destination_key_y_parity, 2 | 3)
            })
        {
            return Err(FastRuntimeError::InvalidConfiguration);
        }
        if self.member_transports.get(&self.signer.address()) != Some(&self.local_transport) {
            return Err(FastRuntimeError::InvalidConfiguration);
        }
        Ok(())
    }
}

/// Explicit C4 operator resources. Finalized Portal reads still supply every roster/domain; these
/// configured peer bindings and endpoints are routing inputs checked against those rosters.
#[derive(Clone, Debug)]
pub struct FastServiceRuntimeConfig {
    pub l1_chain_id: u64,
    pub peers: Vec<FastServicePeerConfig>,
    pub routes: Vec<FastServiceRouteConfig>,
    pub limits: ProtocolLimits,
    pub reserved_terminal_bytes: usize,
    pub health_max_age: Duration,
    pub response_timeout: Duration,
    pub native_rpc_endpoint: String,
    pub operator_signer: PrivateKeySigner,
    pub native_chain_id: u64,
    pub fee_token: Address,
    pub commit_timeout: Duration,
}

impl FastServiceRuntimeConfig {
    fn is_valid(&self) -> bool {
        self.l1_chain_id != 0
            && self.peers.len() == 30
            && self.routes.len() == 9
            && !self.native_rpc_endpoint.is_empty()
            && self.native_chain_id != 0
            && !self.operator_signer.address().is_zero()
            && self.fee_token != Address::ZERO
            && !self.health_max_age.is_zero()
            && !self.response_timeout.is_zero()
            && !self.commit_timeout.is_zero()
    }
}

#[derive(Clone, Debug)]
pub struct FastServicePeerConfig {
    pub zone_id: u32,
    pub certificate_member: Address,
    pub ed25519: P2pPeerId,
    pub endpoint: String,
}

#[derive(Clone, Debug)]
pub struct FastServiceRouteConfig {
    pub zone_id: u32,
    pub chain_id: u64,
    pub portal: Address,
    pub remote_quote: zone_primitives::fast_transfer::QuoteCertificate,
    pub local_quote: zone_primitives::fast_transfer::QuoteCertificate,
    pub outgoing_caps: ValueCaps,
    pub incoming_caps: ValueCaps,
}

async fn read_fast_epoch_config(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    epoch: u64,
    block: alloy_rpc_types_eth::BlockId,
) -> Result<FinalizedPortalEpochConfig, FastRuntimeError> {
    let mut calldata = vec![0u8; 36];
    calldata[..4].copy_from_slice(&keccak256("fastEpochConfig(uint64)")[..4]);
    calldata[28..].copy_from_slice(&epoch.to_be_bytes());
    let output = CallBuilder::new_raw(provider, calldata.into())
        .to(portal)
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if output.len() != FAST_EPOCH_CONFIG_WORDS * 32 {
        return Err(FastRuntimeError::InvalidFinalizedEpochEncoding);
    }
    let word = |index: usize| &output[index * 32..(index + 1) * 32];
    let u8_word = |index: usize| -> Result<u8, FastRuntimeError> {
        let value = word(index);
        if value[..31].iter().any(|byte| *byte != 0) {
            return Err(FastRuntimeError::InvalidFinalizedEpochEncoding);
        }
        Ok(value[31])
    };
    let u32_word = |index: usize| -> Result<u32, FastRuntimeError> {
        let value = word(index);
        if value[..28].iter().any(|byte| *byte != 0) {
            return Err(FastRuntimeError::InvalidFinalizedEpochEncoding);
        }
        Ok(u32::from_be_bytes(
            value[28..].try_into().expect("four-byte word suffix"),
        ))
    };
    let u64_word = |index: usize| -> Result<u64, FastRuntimeError> {
        let value = word(index);
        if value[..24].iter().any(|byte| *byte != 0) {
            return Err(FastRuntimeError::InvalidFinalizedEpochEncoding);
        }
        Ok(u64::from_be_bytes(
            value[24..].try_into().expect("eight-byte word suffix"),
        ))
    };
    let bool_word = |index: usize| -> Result<bool, FastRuntimeError> {
        match u8_word(index)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(FastRuntimeError::InvalidFinalizedEpochEncoding),
        }
    };
    let config = FinalizedPortalEpochConfig {
        protocol_version: u32_word(0)?,
        threshold: u8_word(1)?,
        proof_mode: u8_word(2)?,
        closed: bool_word(3)?,
        retired: bool_word(4)?,
        activated_at_tempo_block: u64_word(8)?,
        roster_hash: B256::from_slice(word(9)),
        peers_hash: B256::from_slice(word(10)),
        expected_verifier_code_hash: B256::from_slice(word(11)),
        expected_verifier_config_hash: B256::from_slice(word(12)),
    };
    validate_fast_epoch_policy(&config)?;
    Ok(config)
}

async fn validate_enrolled_verifier(
    provider: &DynProvider<TempoNetwork>,
    portal_address: Address,
    block: alloy_rpc_types_eth::BlockId,
    config: &FinalizedPortalEpochConfig,
) -> Result<(), FastRuntimeError> {
    let verifier = ZonePortal::new(portal_address, provider)
        .verifier()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    let code = provider
        .get_code_at(verifier)
        .block_id(block)
        .await
        .map_err(l1_error)?;
    if verifier.is_zero() || keccak256(&code) != config.expected_verifier_code_hash {
        return Err(FastRuntimeError::InvalidFinalizedProofPolicy);
    }
    Ok(())
}

fn validate_fast_epoch_policy(config: &FinalizedPortalEpochConfig) -> Result<(), FastRuntimeError> {
    if !matches!(
        config.proof_mode,
        PROOF_MODE_OPERATOR_ATTESTED | PROOF_MODE_REQUIRED
    ) || config.expected_verifier_code_hash.is_zero()
        || config.expected_verifier_config_hash.is_zero()
    {
        return Err(FastRuntimeError::InvalidFinalizedProofPolicy);
    }
    if config.proof_mode == PROOF_MODE_REQUIRED {
        let prototype = keccak256(&tempo_contracts::zones::T13_ZONE_VERIFIER_RUNTIME);
        if config.expected_verifier_code_hash == prototype
            || config.expected_verifier_code_hash == DEVELOPMENT_PROTOTYPE_VERIFIER_CODE_HASH
        {
            return Err(FastRuntimeError::InvalidFinalizedProofPolicy);
        }
    }
    Ok(())
}

fn finalized_roster_hash(
    portal: Address,
    epoch: u64,
    config: &FinalizedPortalEpochConfig,
    members: [Address; 3],
    peers: [Address; 9],
) -> B256 {
    keccak256(
        (
            keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
            portal,
            epoch,
            config.protocol_version,
            alloy_primitives::U256::from(config.threshold),
            alloy_primitives::U256::from(config.proof_mode),
            config.expected_verifier_code_hash,
            config.expected_verifier_config_hash,
            members.to_vec(),
            peers.to_vec(),
        )
            .abi_encode(),
    )
}

/// Resolve every C4 authority value from the same imported canonical Tempo anchor used for the
/// local activation. Configured endpoints and Ed25519 keys remain routing data only.
pub async fn load_fast_service_config(
    l1: &DynProvider<TempoNetwork>,
    imported_anchor: NumHash,
    activation: &FastActivation,
    local_member: Address,
    configured: &FastServiceRuntimeConfig,
) -> Result<FastServiceConfig, FastRuntimeError> {
    let epoch = activation.epoch();
    if configured.l1_chain_id != epoch.l1_chain_id
        || configured.native_chain_id != epoch.zone_chain_id
    {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    let protocol_version = u16::try_from(epoch.protocol_version)
        .map_err(|_| FastRuntimeError::InvalidConfiguration)?;
    let local_roster = EpochRoster::from_finalized_registry(
        ZoneDomain {
            l1_chain_id: epoch.l1_chain_id,
            zone_id: epoch.zone_id,
            chain_id: epoch.zone_chain_id,
            portal: epoch.portal,
            authority_epoch: epoch.epoch,
            roster_hash: epoch.roster_hash,
            protocol_version,
        },
        epoch.members,
    )
    .map_err(|_| FastRuntimeError::InvalidFinalizedRoster)?;
    let mut configured_peers = configured.peers.iter().cloned().fold(
        BTreeMap::<u32, Vec<FastServicePeerConfig>>::new(),
        |mut peers, peer| {
            peers.entry(peer.zone_id).or_default().push(peer);
            peers
        },
    );
    let local_endpoints = service_endpoints(
        configured_peers
            .remove(&epoch.zone_id)
            .ok_or(FastRuntimeError::InvalidConfiguration)?,
        &local_roster,
    )?;
    let mut routes = BTreeMap::new();
    let mut value_caps = HashMap::new();
    let pinned = alloy_rpc_types_eth::BlockId::hash_canonical(imported_anchor.hash);
    let configured_portals = epoch
        .peer_portals
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut network_portals = configured_portals.clone();
    network_portals.insert(epoch.portal);
    let route_portals = configured
        .routes
        .iter()
        .map(|route| route.portal)
        .collect::<std::collections::BTreeSet<_>>();
    if route_portals != configured_portals {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    for route in configured.routes.iter().cloned() {
        if !configured_portals.contains(&route.portal) || route.portal == epoch.portal {
            return Err(FastRuntimeError::InvalidConfiguration);
        }
        let roster = load_remote_roster(
            l1,
            pinned,
            configured.l1_chain_id,
            route.zone_id,
            route.chain_id,
            route.portal,
            protocol_version,
            &network_portals,
            imported_anchor.number,
        )
        .await?;
        let endpoints = service_endpoints(
            configured_peers
                .remove(&route.zone_id)
                .ok_or(FastRuntimeError::InvalidConfiguration)?,
            &roster,
        )?;
        let outgoing_key = RouteKey {
            source_zone: epoch.zone_id,
            destination_zone: route.zone_id,
            l1_token: route.remote_quote.quote.asset.l1_token,
        };
        let incoming_key = RouteKey {
            source_zone: route.zone_id,
            destination_zone: epoch.zone_id,
            l1_token: route.local_quote.quote.asset.l1_token,
        };
        if value_caps
            .insert(outgoing_key, route.outgoing_caps)
            .is_some()
            || value_caps
                .insert(incoming_key, route.incoming_caps)
                .is_some()
            || routes
                .insert(
                    route.zone_id,
                    FastServiceRoute {
                        roster,
                        endpoints,
                        remote_quote: route.remote_quote,
                        local_quote: route.local_quote,
                    },
                )
                .is_some()
        {
            return Err(FastRuntimeError::InvalidConfiguration);
        }
    }
    if !configured_peers.is_empty() || routes.len() != 9 {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    let mut rng = OsRng;
    let stream = loop {
        let candidate = rng.next_u64();
        if candidate != 0 {
            break candidate;
        }
    };
    let service = FastServiceConfig {
        local_roster,
        local_member,
        local_endpoints,
        stream,
        routes,
        limits: configured.limits.clone(),
        value_caps,
        reserved_terminal_bytes: configured.reserved_terminal_bytes,
        health_max_age: configured.health_max_age,
    };
    service
        .validate()
        .map_err(|_| FastRuntimeError::InvalidConfiguration)?;
    Ok(service)
}

fn service_endpoints(
    peers: Vec<FastServicePeerConfig>,
    roster: &EpochRoster,
) -> Result<[PeerEndpoint; 3], FastRuntimeError> {
    let endpoints: Vec<_> = peers
        .into_iter()
        .map(|peer| PeerEndpoint {
            member: peer.certificate_member,
            ed25519: peer.ed25519,
            endpoint: peer.endpoint,
        })
        .collect();
    let endpoints: [PeerEndpoint; 3] = endpoints
        .try_into()
        .map_err(|_| FastRuntimeError::InvalidConfiguration)?;
    let members = endpoints
        .iter()
        .map(|endpoint| endpoint.member)
        .collect::<std::collections::BTreeSet<_>>();
    if members != roster.members.into_iter().collect() {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    Ok(endpoints)
}

async fn load_remote_roster(
    l1: &DynProvider<TempoNetwork>,
    block: alloy_rpc_types_eth::BlockId,
    l1_chain_id: u64,
    zone_id: u32,
    chain_id: u64,
    portal_address: Address,
    protocol_version: u16,
    network_portals: &std::collections::BTreeSet<Address>,
    imported_anchor_number: u64,
) -> Result<EpochRoster, FastRuntimeError> {
    let portal = ZonePortal::new(portal_address, l1);
    if portal
        .zoneId()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?
        != zone_id
        || zone_primitives::constants::zone_chain_id(l1_chain_id, zone_id)
            .map_err(|_| FastRuntimeError::InvalidFinalizedRoster)?
            != chain_id
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let epoch = portal
        .fastEpoch()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if epoch == 0 {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let config = read_fast_epoch_config(l1, portal_address, epoch, block).await?;
    validate_enrolled_verifier(l1, portal_address, block, &config).await?;
    if config.retired
        || config.protocol_version != u32::from(protocol_version)
        || config.threshold != 2
        || config.activated_at_tempo_block == 0
        || config.activated_at_tempo_block > imported_anchor_number
        || portal
            .fastEpochMemberCount(epoch)
            .block(block)
            .call()
            .await
            .map_err(l1_error)?
            != alloy_primitives::U256::from(3)
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let mut members = [Address::ZERO; 3];
    for (index, member) in members.iter_mut().enumerate() {
        *member = portal
            .fastEpochMemberAt(epoch, alloy_primitives::U256::from(index))
            .block(block)
            .call()
            .await
            .map_err(l1_error)?;
    }
    if portal
        .fastEpochPeerCount(epoch)
        .block(block)
        .call()
        .await
        .map_err(l1_error)?
        != alloy_primitives::U256::from(9)
    {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    let mut peer_portals = Vec::with_capacity(9);
    for index in 0..9 {
        peer_portals.push(
            portal
                .fastEpochPeerAt(epoch, alloy_primitives::U256::from(index))
                .block(block)
                .call()
                .await
                .map_err(l1_error)?,
        );
    }
    let peer_portals: [Address; 9] = peer_portals
        .try_into()
        .map_err(|_| FastRuntimeError::InvalidFinalizedPeers)?;
    let peers = peer_portals.iter().copied().collect();
    let expected_peers = network_portals
        .iter()
        .copied()
        .filter(|candidate| *candidate != portal_address)
        .collect::<std::collections::BTreeSet<_>>();
    if peers != expected_peers {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    if config.peers_hash != keccak256(peer_portals.to_vec().abi_encode()) {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    if config.roster_hash
        != finalized_roster_hash(portal_address, epoch, &config, members, peer_portals)
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    EpochRoster::from_finalized_registry(
        ZoneDomain {
            l1_chain_id,
            zone_id,
            chain_id,
            portal: portal_address,
            authority_epoch: epoch,
            roster_hash: config.roster_hash,
            protocol_version,
        },
        members,
    )
    .map_err(|_| FastRuntimeError::InvalidFinalizedRoster)
}

/// One explicit, independently preflighted operator-inventory route. Provider handles contain the
/// configured source, destination, and L1 treasury endpoints and wallets; no endpoint or signer is
/// inferred from the public RPC or a peer manifest.
#[derive(Clone)]
pub struct FastReplenishmentRuntimeConfig {
    pub route: ReplenishmentRouteConfig,
    pub providers: AlloyReplenishmentProviderHandles,
    pub expected_source_fast_epoch: u64,
    pub expected_destination_fast_epoch: u64,
    pub expected_token_decimals: u8,
    pub expected_destination_key_index: alloy_primitives::U256,
    pub expected_destination_key_x: B256,
    pub expected_destination_key_y_parity: u8,
    pub storage: PathBuf,
    pub poll_interval: Duration,
}

impl std::fmt::Debug for FastReplenishmentRuntimeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FastReplenishmentRuntimeConfig")
            .field("route", &self.route)
            .field(
                "expected_source_fast_epoch",
                &self.expected_source_fast_epoch,
            )
            .field(
                "expected_destination_fast_epoch",
                &self.expected_destination_fast_epoch,
            )
            .field("expected_token_decimals", &self.expected_token_decimals)
            .field(
                "expected_destination_key_index",
                &self.expected_destination_key_index,
            )
            .field(
                "expected_destination_key_x",
                &self.expected_destination_key_x,
            )
            .field(
                "expected_destination_key_y_parity",
                &self.expected_destination_key_y_parity,
            )
            .field("storage", &self.storage)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

/// Read the complete capability at one finalized L1 block. Calls are explicitly block-pinned so
/// a moving RPC head cannot splice fields from different epochs.
pub async fn load_fast_activation(
    l1: &DynProvider<TempoNetwork>,
    portal_address: Address,
    chain_spec: &ZoneChainSpec,
    imported_anchor: NumHash,
    zone_id: u32,
    zone_chain_id: u64,
) -> Result<Option<LoadedFastActivation>, FastRuntimeError> {
    let finalized = l1
        .get_header_by_number(BlockNumberOrTag::Finalized)
        .await
        .map_err(l1_error)?
        .ok_or(FastRuntimeError::NoFinalizedHeader)?;
    if finalized.number() < imported_anchor.number {
        return Err(FastRuntimeError::FinalizedHeadBehindAnchor {
            finalized: finalized.number(),
            anchor: imported_anchor.number,
        });
    }
    let anchor = l1
        .get_header_by_hash(imported_anchor.hash)
        .await
        .map_err(l1_error)?
        .ok_or(FastRuntimeError::ImportedAnchorUnavailable(imported_anchor))?;
    if anchor.number() != imported_anchor.number || anchor.hash_slow() != imported_anchor.hash {
        return Err(FastRuntimeError::ImportedAnchorMismatch {
            imported: imported_anchor,
            remote: NumHash::new(anchor.number(), anchor.hash_slow()),
        });
    }
    if !chain_spec.supports_same_anchor_at(anchor.timestamp()) {
        return Ok(None);
    }
    // The Zone's imported TempoState is the sole capability anchor. Hash-canonical pinning keeps
    // every registry field on that exact state even if the remote finalized head advances.
    let block = alloy_rpc_types_eth::BlockId::hash_canonical(imported_anchor.hash);
    let portal = ZonePortal::new(portal_address, l1);
    let epoch = portal
        .fastEpoch()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if epoch == 0 {
        return Ok(None);
    }
    let native_pin = portal
        .FAST_PROTOCOL_NATIVE_PIN()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    let config = read_fast_epoch_config(l1, portal_address, epoch, block).await?;
    validate_enrolled_verifier(l1, portal_address, block, &config).await?;
    let member_count = portal
        .fastEpochMemberCount(epoch)
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if member_count != alloy_primitives::U256::from(3) {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let mut members = [Address::ZERO; 3];
    for (index, member) in members.iter_mut().enumerate() {
        *member = portal
            .fastEpochMemberAt(epoch, alloy_primitives::U256::from(index))
            .block(block)
            .call()
            .await
            .map_err(l1_error)?;
    }
    let peer_count = portal
        .fastEpochPeerCount(epoch)
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if peer_count != alloy_primitives::U256::from(9) {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    let mut peer_portals = [Address::ZERO; 9];
    for (index, peer) in peer_portals.iter_mut().enumerate() {
        *peer = portal
            .fastEpochPeerAt(epoch, alloy_primitives::U256::from(index))
            .block(block)
            .call()
            .await
            .map_err(l1_error)?;
    }
    if config.peers_hash != keccak256(peer_portals.to_vec().abi_encode()) {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    if config.roster_hash
        != finalized_roster_hash(portal_address, epoch, &config, members, peer_portals)
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let activated_at_l1_block = config.activated_at_tempo_block;
    let activation = FastActivation::from_finalized_epoch(FinalizedT14Capability {
        epoch: FinalizedFastEpoch {
            l1_chain_id: l1.get_chain_id().await.map_err(l1_error)?,
            portal: portal_address,
            zone_id,
            zone_chain_id,
            epoch,
            protocol_version: config.protocol_version,
            threshold: config.threshold,
            proof_mode: config.proof_mode,
            expected_verifier_code_hash: config.expected_verifier_code_hash,
            expected_verifier_config_hash: config.expected_verifier_config_hash,
            members,
            peer_portals,
            roster_hash: config.roster_hash,
            finalized_l1_block: imported_anchor.number,
        },
        native_pin,
        t14_active_at_anchor: true,
        current_epoch: epoch,
        activated_at_l1_block,
        closed: config.closed,
        retired: config.retired,
    })?;
    Ok(Some(LoadedFastActivation {
        activation,
        admission_open: !config.closed,
        proof_policy: FinalizedFastProofPolicy {
            mode: config.proof_mode,
            expected_verifier_code_hash: config.expected_verifier_code_hash,
            expected_verifier_config_hash: config.expected_verifier_config_hash,
        },
        allow_initialize: activated_at_l1_block == imported_anchor.number,
        anchor_timestamp: anchor.timestamp(),
        anchor_timestamp_millis_part: (anchor.timestamp_millis % 1_000) as u16,
    }))
}

fn enrollment_sentinel(epoch: &FinalizedFastEpoch, proof_policy: FinalizedFastProofPolicy) -> B256 {
    let mut encoded = Vec::with_capacity(256);
    encoded.extend_from_slice(ENROLLMENT_SENTINEL_DOMAIN);
    encoded.extend_from_slice(&epoch.l1_chain_id.to_be_bytes());
    encoded.extend_from_slice(epoch.portal.as_slice());
    encoded.extend_from_slice(&epoch.zone_id.to_be_bytes());
    encoded.extend_from_slice(&epoch.zone_chain_id.to_be_bytes());
    encoded.extend_from_slice(&epoch.epoch.to_be_bytes());
    encoded.extend_from_slice(&epoch.protocol_version.to_be_bytes());
    encoded.push(epoch.threshold);
    for member in epoch.members {
        encoded.extend_from_slice(member.as_slice());
    }
    for portal in epoch.peer_portals {
        encoded.extend_from_slice(portal.as_slice());
    }
    encoded.extend_from_slice(epoch.roster_hash.as_slice());
    encoded.push(proof_policy.mode);
    encoded.extend_from_slice(proof_policy.expected_verifier_code_hash.as_slice());
    encoded.extend_from_slice(proof_policy.expected_verifier_config_hash.as_slice());
    keccak256(encoded)
}

fn persist_or_validate_enrollment(
    storage: &std::path::Path,
    epoch: &FinalizedFastEpoch,
    proof_policy: FinalizedFastProofPolicy,
    may_create: bool,
) -> Result<(), FastRuntimeError> {
    let path = storage.join(ENROLLMENT_SENTINEL);
    let expected = enrollment_sentinel(epoch, proof_policy);
    match std::fs::read(&path) {
        Ok(actual) if actual.as_slice() == expected.as_slice() && !may_create => return Ok(()),
        Ok(actual) if actual.as_slice() == expected.as_slice() => {
            return Err(FastRuntimeError::EnrollmentWithoutState(path));
        }
        Ok(_) => return Err(FastRuntimeError::EnrollmentConflict(path)),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) if !may_create => return Err(FastRuntimeError::EnrollmentMissing(path)),
        Err(_) => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(expected.as_slice())?;
    file.sync_all()?;
    std::fs::File::open(storage)?.sync_all()?;
    Ok(())
}

/// Assemble durable stores, authenticated transport, peer handlers, and exact three-member
/// OpenRaft membership. Existing Zone state may never silently acquire an empty Raft history.
pub async fn assemble_production_fast_runtime<E, P>(
    activation: FastActivation,
    proof_policy: FinalizedFastProofPolicy,
    allow_initialize: bool,
    config: FastRuntimeConfig,
    provider: &P,
    executor: Arc<E>,
    commands: mpsc::Sender<P2pCommand>,
    raft_ports: RaftPorts,
) -> Result<AssembledFastRuntime<E>, FastRuntimeError>
where
    E: DurableStateMachineExecution,
    P: BlockNumReader + BlockReader<Block = tempo_primitives::Block>,
{
    config.validate()?;
    let epoch = activation.epoch();
    let local_index = epoch
        .members
        .iter()
        .position(|member| *member == config.signer.address())
        .ok_or(FastRuntimeError::LocalMemberNotEnrolled)?;
    let log_exists = config.storage.join("log/raft-log.bin").is_file();
    let state_machine_exists = config
        .storage
        .join("state-machine/raft-state-machine.bin")
        .is_file();
    if log_exists != state_machine_exists {
        return Err(FastRuntimeError::IncompleteEnrolledStorage(config.storage));
    }
    let has_state = log_exists && state_machine_exists;
    let best = provider
        .best_block_number()
        .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
    let already_produced_fast_block = allow_initialize
        && provider
            .block_by_number(best)
            .map_err(|error| FastRuntimeError::Provider(error.to_string()))?
            .is_some_and(|block| {
                block.body.transactions.first().is_some_and(|transaction| {
                    zone_evm::same_anchor::is_same_anchor_opening(transaction.input())
                })
            });
    if (!allow_initialize || already_produced_fast_block) && !has_state {
        return Err(FastRuntimeError::MissingEnrolledStorage(config.storage));
    }
    let members = epoch
        .members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            let node_id = u64::try_from(index + 1).expect("three members fit u64");
            let transport = config
                .member_transports
                .get(member)
                .cloned()
                .ok_or(FastRuntimeError::EndpointMissing(*member))?;
            Ok((
                node_id,
                FinalizedPeerIdentity {
                    member: *member,
                    transport,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>, FastRuntimeError>>()?;
    let local_node_id = u64::try_from(local_index + 1).expect("three members fit u64");
    let RaftPorts {
        requests,
        responses,
    } = raft_ports;
    let network_config = FastNetworkConfig {
        epoch: epoch.epoch,
        local_node_id,
        local_member: config.signer.address(),
        local_transport: config.local_transport,
        members,
        rpc_timeout: config.rpc_timeout,
    };
    network_config.validate()?;
    let raft_config = Arc::new(
        Config {
            cluster_name: format!("tempo-zone-{}-fast-epoch-{}", epoch.zone_id, epoch.epoch),
            ..Config::default()
        }
        .validate()?,
    );
    std::fs::create_dir_all(&config.storage)?;
    persist_or_validate_enrollment(&config.storage, epoch, proof_policy, !has_state)?;
    let transport = AuthenticatedRaftTransport::new(network_config, commands.clone(), responses)?;
    let runtime = Arc::new(
        assemble_fast_raft(
            &activation,
            transport.config().local_member,
            raft_config,
            Arc::new(transport.clone()),
            &config.storage,
            executor,
        )
        .await?,
    );
    let membership = epoch
        .members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            (
                u64::try_from(index + 1).expect("three members fit u64"),
                BasicNode::new(
                    config
                        .member_transports
                        .get(member)
                        .expect("validated endpoint exists")
                        .to_string(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let needs_initialize = !runtime
        .raft
        .inner()
        .is_initialized()
        .await
        .map_err(|error| FastRuntimeError::Initialize(error.to_string()))?;
    Ok(AssembledFastRuntime {
        runtime,
        transport,
        commands,
        requests,
        local_node_id,
        epoch: epoch.epoch,
        initial_membership: membership,
        needs_initialize,
    })
}

pub struct AssembledFastRuntime<E: DurableStateMachineExecution> {
    pub runtime: Arc<FastRaftRuntime<E>>,
    pub transport: AuthenticatedRaftTransport,
    pub local_node_id: u64,
    pub epoch: u64,
    pub commands: mpsc::Sender<P2pCommand>,
    pub requests: mpsc::Receiver<RaftRequestFrame>,
    pub initial_membership: BTreeMap<u64, BasicNode>,
    pub needs_initialize: bool,
}

/// Bootstrap exactly once, from finalized roster member 1, after all authenticated peer handlers
/// are serving. Followers acquire the initial membership only through AppendEntries.
pub async fn initialize_production_fast_runtime<E: DurableStateMachineExecution>(
    runtime: &FastRaftRuntime<E>,
    local_node_id: u64,
    needs_initialize: bool,
    membership: BTreeMap<u64, BasicNode>,
) -> Result<(), FastRuntimeError> {
    if needs_initialize && local_node_id == 1 {
        runtime
            .raft
            .inner()
            .initialize(membership)
            .await
            .map_err(|error| FastRuntimeError::Initialize(error.to_string()))?;
    }
    Ok(())
}

pub trait LocalOutcomeSigner: Send + Sync + 'static {
    fn sign_local(&self, transfer_id: B256) -> Result<SignedOutcome, String>;
}

/// One production handler owns both consensus RPCs and the local committed-outcome signer.
pub struct ProductionPeerHandler<E: DurableStateMachineExecution> {
    runtime: Arc<FastRaftRuntime<E>>,
    signer: Arc<dyn LocalOutcomeSigner>,
}

impl<E: DurableStateMachineExecution> ProductionPeerHandler<E> {
    pub fn new(runtime: Arc<FastRaftRuntime<E>>, signer: Arc<dyn LocalOutcomeSigner>) -> Self {
        Self { runtime, signer }
    }
}

impl<E: DurableStateMachineExecution> FastRaftPeerHandler for ProductionPeerHandler<E> {
    fn append_entries(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::AppendEntriesRequest<crate::fast_quorum::FastRaftConfig>,
    ) -> HandlerFuture<'_, openraft::raft::AppendEntriesResponse<u64>> {
        Box::pin(async move {
            self.runtime
                .handle_append_entries(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn vote(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::VoteRequest<u64>,
    ) -> HandlerFuture<'_, openraft::raft::VoteResponse<u64>> {
        Box::pin(async move {
            self.runtime
                .handle_vote(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn install_snapshot(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::InstallSnapshotRequest<crate::fast_quorum::FastRaftConfig>,
    ) -> HandlerFuture<'_, openraft::raft::InstallSnapshotResponse<u64>> {
        Box::pin(async move {
            self.runtime
                .handle_install_snapshot(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn sign_outcome(
        &self,
        _peer: AuthenticatedRaftPeer,
        transfer_id: B256,
    ) -> HandlerFuture<'_, SignedOutcome> {
        Box::pin(async move { self.signer.sign_local(transfer_id) })
    }
}

pub struct ProductionOutcomeCertification<P> {
    execution: Arc<CanonicalFastExecution<P>>,
    activation: FastActivation,
    signer: PrivateKeySigner,
    transport: AuthenticatedRaftTransport,
    local_node_id: u64,
    certification_lock: tokio::sync::Mutex<()>,
}

/// Runtime-owned committed facade used by the C4 service. Every read is fenced by the fsynced
/// state-machine image and certificate recovery uses the production quorum path.
pub struct ProductionCommittedTransferSource<P> {
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
    certification: Arc<ProductionOutcomeCertification<P>>,
}

impl<P> ProductionCommittedTransferSource<P> {
    pub const fn new(
        committed: CommittedStateHandle<CanonicalFastExecution<P>>,
        certification: Arc<ProductionOutcomeCertification<P>>,
    ) -> Self {
        Self {
            committed,
            certification,
        }
    }
}

impl<P> CommittedTransferSource for ProductionCommittedTransferSource<P>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn committed_transfers(&self) -> Result<Vec<CommittedTransferRecord>, FastServiceError> {
        self.committed
            .committed_transfers()
            .map_err(|error| FastServiceError::CommittedState(error.to_string()))
    }

    fn committed_transfer(
        &self,
        transfer_id: B256,
    ) -> Result<Option<CommittedTransferRecord>, FastServiceError> {
        self.committed
            .committed_transfer(transfer_id)
            .map_err(|error| FastServiceError::CommittedState(error.to_string()))
    }

    fn committed_height(&self) -> Result<u64, FastServiceError> {
        self.committed
            .committed_head()
            .map_err(|error| FastServiceError::CommittedState(error.to_string()))?
            .map(|head| head.block.block_height)
            .ok_or_else(|| {
                FastServiceError::CommittedState("committed fast head is unavailable".to_owned())
            })
    }

    fn committed_transaction_result(
        &self,
        transaction_hash: B256,
    ) -> Result<Option<bool>, FastServiceError> {
        self.committed
            .committed_transaction_result(transaction_hash)
            .map_err(|error| FastServiceError::CommittedState(error.to_string()))
    }

    fn ensure_certificate<'a>(
        &'a self,
        record: &'a CommittedTransferRecord,
    ) -> ServiceFuture<'a, Result<OutcomeCertificate, FastServiceError>> {
        Box::pin(async move {
            self.certification
                .ensure_transfer_certificate(record.body.transfer_id)
                .await
                .map_err(FastServiceError::CommittedState)
        })
    }
}

impl<P> ProductionOutcomeCertification<P> {
    pub fn new(
        execution: Arc<CanonicalFastExecution<P>>,
        activation: FastActivation,
        signer: PrivateKeySigner,
        transport: AuthenticatedRaftTransport,
        local_node_id: u64,
    ) -> Self {
        Self {
            execution,
            activation,
            signer,
            transport,
            local_node_id,
            certification_lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl<P> ProductionOutcomeCertification<P>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Return the durable certificate for one committed transfer, recollecting the exact original
    /// `(term, index)` body when a crash interrupted assembly after state-machine application.
    pub async fn ensure_transfer_certificate(
        &self,
        transfer_id: B256,
    ) -> Result<OutcomeCertificate, String> {
        let record = self
            .execution
            .committed_transfer(transfer_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "committed transfer is unavailable".to_owned())?;
        if let Some(certificate) = record.certificate {
            return Ok(certificate);
        }
        let commit = RaftCommit {
            term: record.body.log_term,
            index: record.body.log_index,
            block: crate::fast_quorum::CommittedBlock {
                input_digest: B256::ZERO,
                block_height: record.body.block_height,
                block_hash: record.body.block_hash,
                state_root: record.body.state_root,
                receipts_root: B256::ZERO,
            },
        };
        self.certify_commit(commit).await?;
        self.execution
            .committed_transfer(transfer_id)
            .map_err(|error| error.to_string())?
            .and_then(|record| record.certificate)
            .ok_or_else(|| "committed transfer certificate was not durably assembled".to_owned())
    }

    /// Recollect any certificate interrupted after commit, preserving the original term/index.
    pub async fn certify_pending(&self) -> Result<(), String> {
        for commit in self
            .execution
            .uncertified_commits()
            .map_err(|error| error.to_string())?
        {
            self.certify_commit(commit).await?;
        }
        Ok(())
    }
}

impl<P> LocalOutcomeSigner for ProductionOutcomeCertification<P>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn sign_local(&self, transfer_id: B256) -> Result<SignedOutcome, String> {
        let (body, signature) = self
            .execution
            .sign_committed_transfer(&self.activation, &self.signer, transfer_id)
            .map_err(|error| error.to_string())?;
        Ok(SignedOutcome {
            body: body.canonical_bytes(),
            signature: signature.0.to_vec(),
        })
    }
}

impl<P> FastOutcomeCertification for ProductionOutcomeCertification<P>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn certify_commit(&self, commit: RaftCommit) -> CertificationFuture<'_> {
        Box::pin(async move {
            let _guard = self.certification_lock.lock().await;
            let records = self
                .execution
                .outcomes_for_commit(commit.term, commit.index)
                .map_err(|error| error.to_string())?;
            let epoch = self.activation.epoch();
            let protocol_version = u16::try_from(epoch.protocol_version)
                .map_err(|_| "invalid finalized protocol version".to_owned())?;
            let verifier = QuorumVerifier::new(
                EpochRoster::from_finalized_registry(
                    ZoneDomain {
                        l1_chain_id: epoch.l1_chain_id,
                        zone_id: epoch.zone_id,
                        chain_id: epoch.zone_chain_id,
                        portal: epoch.portal,
                        authority_epoch: epoch.epoch,
                        roster_hash: epoch.roster_hash,
                        protocol_version,
                    },
                    epoch.members,
                )
                .map_err(|error| error.to_string())?,
            );
            for record in records {
                if record.certificate.is_some() {
                    continue;
                }
                let local = self.sign_local(record.body.transfer_id)?;
                let local_signature = signature_from_wire(&local.signature)?;
                let mut remote_signature = None;
                let remote_nodes = self
                    .transport
                    .config()
                    .members
                    .keys()
                    .copied()
                    .filter(|node_id| *node_id != self.local_node_id)
                    .collect::<Vec<_>>();
                let deadline = tokio::time::Instant::now() + self.transport.config().rpc_timeout;
                while remote_signature.is_none() && tokio::time::Instant::now() < deadline {
                    let (first, second) = tokio::join!(
                        self.transport
                            .request_outcome_signature(remote_nodes[0], record.body.transfer_id),
                        self.transport
                            .request_outcome_signature(remote_nodes[1], record.body.transfer_id)
                    );
                    for remote in [first, second].into_iter().flatten() {
                        if decode_exact::<CertificateBody>(&remote.body, MAX_CERTIFICATE_BYTES)
                            .ok()
                            .as_ref()
                            == Some(&record.body)
                            && let Ok(signature) = signature_from_wire(&remote.signature)
                        {
                            let candidate = OutcomeCertificate {
                                body: record.body.clone(),
                                signatures: [local_signature, signature],
                            };
                            if verify_committed_certificate(
                                &verifier,
                                &candidate,
                                &record.intent,
                                &commit,
                            )
                            .is_ok()
                            {
                                remote_signature = Some(signature);
                                break;
                            }
                        }
                    }
                    if remote_signature.is_none() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                let certificate = OutcomeCertificate {
                    body: record.body.clone(),
                    signatures: [
                        local_signature,
                        remote_signature.ok_or_else(|| {
                            "no second finalized member signed the committed outcome".to_owned()
                        })?,
                    ],
                };
                verify_committed_certificate(&verifier, &certificate, &record.intent, &commit)
                    .map_err(|error| error.to_string())?;
                self.execution
                    .persist_certificate(certificate)
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    }
}

fn signature_from_wire(encoded: &[u8]) -> Result<SignatureBytes, String> {
    let bytes: [u8; 65] = encoded
        .try_into()
        .map_err(|_| "invalid outcome signature length".to_owned())?;
    Ok(SignatureBytes(bytes))
}

impl<E: DurableStateMachineExecution> FastRaftPeerHandler for FastRaftRuntime<E> {
    fn append_entries(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::AppendEntriesRequest<crate::fast_quorum::FastRaftConfig>,
    ) -> HandlerFuture<'_, openraft::raft::AppendEntriesResponse<u64>> {
        Box::pin(async move {
            self.handle_append_entries(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn vote(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::VoteRequest<u64>,
    ) -> HandlerFuture<'_, openraft::raft::VoteResponse<u64>> {
        Box::pin(async move {
            self.handle_vote(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn install_snapshot(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::InstallSnapshotRequest<crate::fast_quorum::FastRaftConfig>,
    ) -> HandlerFuture<'_, openraft::raft::InstallSnapshotResponse<u64>> {
        Box::pin(async move {
            self.handle_install_snapshot(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

/// Preflight one C6 route, reconstruct all unallocated released inventory from the committed
/// prefix, and continuously drive only real provider-backed bridge actions.
pub async fn run_replenishment_route<P>(
    config: FastReplenishmentRuntimeConfig,
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
    journal: Arc<DurableJournal>,
    stop: CancellationToken,
) -> Result<(), FastRuntimeError>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    if config.poll_interval.is_zero() || config.storage.as_os_str().is_empty() {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    let actions = Arc::new(FilePreparedActionStore::open(
        config.storage.join("prepared-actions"),
    )?);
    let nonces = Arc::new(
        FileReplenishmentNoncePlanner::open(
            config.storage.join("nonces.json"),
            &config.providers.source_zone,
            config.providers.source_signing.sender,
            &config.providers.tempo_l1_treasury,
            config.providers.treasury_signing.sender,
        )
        .await?,
    );
    let bridge = ProviderBackedReplenishmentBridge::enable(
        config.route.clone(),
        Arc::new(config.providers),
        actions,
        nonces,
    )
    .await?;
    let finalized = bridge.finalized_route();
    if finalized.source_fast_epoch != config.expected_source_fast_epoch
        || finalized.destination_fast_epoch != config.expected_destination_fast_epoch
        || finalized.l1_token_decimals != config.expected_token_decimals
        || finalized.source_token_decimals != config.expected_token_decimals
        || finalized.destination_token_decimals != config.expected_token_decimals
        || finalized.destination_key_index != config.expected_destination_key_index
        || finalized.destination_key_x != config.expected_destination_key_x
        || finalized.destination_key_y_parity != config.expected_destination_key_y_parity
    {
        return Err(FastRuntimeError::InvalidReplenishmentEnrollment);
    }
    let worker = ReplenishmentWorker::new(journal.clone(), bridge);
    let mut interval = tokio::time::interval(config.poll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {
                reconstruct_replenishment_jobs(
                    &config.route,
                    &committed,
                    journal.as_ref(),
                )?;
                for job in journal.unfinished_replenishment_jobs()? {
                    if job.source_inventory != config.route.source_inventory
                        || job.source_fallback != config.route.source_fallback
                        || job.treasury != config.route.treasury
                        || job.destination_pool != config.route.destination_pool_operator
                    {
                        continue;
                    }
                    // One persist-before-I/O transition per tick keeps every route bounded and
                    // gives canonical observations time to advance.
                    let _ = worker.step(job.job_id).await?;
                }
            }
        }
    }
}

fn reconstruct_replenishment_jobs<P>(
    route: &ReplenishmentRouteConfig,
    committed: &CommittedStateHandle<CanonicalFastExecution<P>>,
    journal: &DurableJournal,
) -> Result<(), FastRuntimeError>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    for record in committed
        .committed_transfers()
        .map_err(|error| FastRuntimeError::CommittedState(error.to_string()))?
    {
        let zone_primitives::fast_transfer::TransferOutcome::Released {
            beneficiary,
            amount,
        } = record.body.outcome
        else {
            continue;
        };
        if record.intent.source.portal != route.source_portal
            || record.intent.destination.portal != route.destination_portal
            || record.intent.asset.l1_token != route.l1_token
            || record.intent.asset.source_token != route.source_token
            || record.intent.asset.destination_token != route.destination_token
            || record.intent.destination_pool != route.destination_pool_operator
            || beneficiary != route.source_inventory
        {
            continue;
        }
        if amount.is_zero() || amount > route.maximum_replenishment_amount {
            return Err(FastRuntimeError::InvalidReplenishmentRecord(
                record.body.transfer_id,
            ));
        }
        let job_id = replenishment_job_id(route, record.body.transfer_id);
        if journal.replenishment_job(job_id)?.is_some() {
            continue;
        }
        let job = ReplenishmentJob::allocate(
            job_id,
            route.source_inventory,
            route.source_fallback,
            route.treasury,
            route.destination_pool_operator,
            record.intent.recipient,
            vec![InventoryContribution {
                transfer_id: record.body.transfer_id,
                amount,
            }],
        )?;
        journal.persist_replenishment_job(job)?;
    }
    Ok(())
}

fn replenishment_job_id(route: &ReplenishmentRouteConfig, transfer_id: B256) -> B256 {
    keccak256(
        (
            keccak256("TEMPO_ZONE_FAST_REPLENISHMENT_JOB_T14_V1"),
            route.l1_chain_id,
            route.source_chain_id,
            route.destination_chain_id,
            route.protocol_version,
            route.source_portal,
            route.destination_portal,
            route.l1_token,
            route.source_token,
            route.destination_token,
            transfer_id,
        )
            .abi_encode(),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum FastRuntimeError {
    #[error(
        "fast runtime configuration must specify exactly three member endpoints including the local listener"
    )]
    InvalidConfiguration,
    #[error("finalized L1 header is unavailable")]
    NoFinalizedHeader,
    #[error(
        "remote finalized L1 head {finalized} is behind the Zone's imported Tempo anchor {anchor}"
    )]
    FinalizedHeadBehindAnchor { finalized: u64, anchor: u64 },
    #[error("the Zone's imported Tempo anchor {0:?} is unavailable from the L1 provider")]
    ImportedAnchorUnavailable(NumHash),
    #[error("L1 provider returned {remote:?} for imported Tempo anchor {imported:?}")]
    ImportedAnchorMismatch { imported: NumHash, remote: NumHash },
    #[error("finalized fast epoch does not have exactly three members")]
    InvalidFinalizedRoster,
    #[error("finalized fast epoch does not have exactly nine peer Portals")]
    InvalidFinalizedPeers,
    #[error("finalized fast epoch config has non-canonical ABI encoding")]
    InvalidFinalizedEpochEncoding,
    #[error("finalized fast epoch has an invalid or unsafe proof policy")]
    InvalidFinalizedProofPolicy,
    #[error("local signing key is not enrolled in the finalized epoch")]
    LocalMemberNotEnrolled,
    #[error("no authenticated manifest identity is configured for finalized member {0}")]
    EndpointMissing(Address),
    #[error("existing enrolled Zone is missing its fast runtime storage at {0}")]
    MissingEnrolledStorage(PathBuf),
    #[error("fast runtime storage has only one of its durable log/state-machine files at {0}")]
    IncompleteEnrolledStorage(PathBuf),
    #[error("existing fast runtime storage is missing its exact enrollment sentinel at {0}")]
    EnrollmentMissing(PathBuf),
    #[error("fast runtime enrollment sentinel conflicts with the finalized epoch at {0}")]
    EnrollmentConflict(PathBuf),
    #[error("fast runtime enrollment sentinel exists but its durable Raft state is missing at {0}")]
    EnrollmentWithoutState(PathBuf),
    #[error("failed to initialize exact three-member Raft cluster: {0}")]
    Initialize(String),
    #[error(transparent)]
    Activation(#[from] crate::fast_quorum::ActivationError),
    #[error(transparent)]
    Assembly(#[from] crate::fast_quorum::AssembleFastRaftError),
    #[error(transparent)]
    Network(#[from] crate::fast_network::FastNetworkError),
    #[error("finalized L1 fast-epoch read failed: {0}")]
    L1(String),
    #[error("Zone provider error: {0}")]
    Provider(String),
    #[error("committed fast state read failed: {0}")]
    CommittedState(String),
    #[error("committed transfer {0} cannot be represented by the configured replenishment route")]
    InvalidReplenishmentRecord(B256),
    #[error(
        "provider-backed replenishment enrollment does not match the configured epochs, asset decimals, or destination encryption key"
    )]
    InvalidReplenishmentEnrollment,
    #[error(transparent)]
    FastExecution(#[from] crate::fast_execution::FastExecutionError),
    #[error(transparent)]
    Journal(#[from] zone_fast_transfer::JournalError),
    #[error(transparent)]
    Replenishment(#[from] zone_fast_transfer::ReplenishmentError),
    #[error(transparent)]
    ReplenishmentWorker(#[from] zone_fast_transfer::ReplenishmentWorkerError),
    #[error(transparent)]
    ReplenishmentProvider(#[from] ReplenishmentProviderError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    RaftConfig(#[from] openraft::ConfigError),
}

fn l1_error(error: impl std::fmt::Display) -> FastRuntimeError {
    FastRuntimeError::L1(error.to_string())
}
