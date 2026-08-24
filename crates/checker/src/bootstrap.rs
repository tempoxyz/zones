//! Authenticates Zone genesis and discovers its Portal creation on Tempo.

use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockId, BlockNumHash};
use alloy_network::{BlockResponse as _, ReceiptResponse as _};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_rpc_types_eth::Filter;
use alloy_sol_types::SolEvent as _;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_storage_api::{BlockNumReader, StateProviderFactory};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TempoHardforks;
use tempo_zone_contracts::{ZONE_FACTORY_ADDRESS, ZoneFactory};

use crate::{
    AttemptError, CheckerConfig,
    accounting::{State, effects},
    decode_event,
    l1::{classify_rpc_error, collect_l1_block, portal_balances},
    l2::read_zone_genesis,
    persistence::{self, Checkpoint},
};

const GENESIS_BLOCK: u64 = 0;
const LOG_QUERY_BLOCKS: u64 = 10_000;
const LOG_QUERY_CONCURRENCY: usize = 8;

/// Authenticated Zone genesis and Portal identity, without replayed accounting state.
pub(crate) struct Bootstrap {
    identity: persistence::Identity,
    zone: persistence::BlockRef,
    tempo: persistence::BlockRef,
    initial_token: Address,
}

impl Bootstrap {
    pub(crate) const fn identity(&self) -> persistence::Identity {
        self.identity
    }

    pub(crate) const fn zone(&self) -> persistence::BlockRef {
        self.zone
    }

    /// Replay the authenticated pre-genesis history only when a database must be created or reset.
    pub(crate) async fn checkpoint(
        &self,
        provider: &DynProvider<TempoNetwork>,
        config: &CheckerConfig,
    ) -> Result<Checkpoint, AttemptError> {
        let creation = BlockNumHash::from(self.identity.creation);
        let tempo = BlockNumHash::from(self.tempo);
        let state = initial_state(provider, config, creation, tempo, self.initial_token).await?;
        Ok(Checkpoint {
            identity: self.identity,
            zone: self.zone,
            tempo: self.tempo,
            state,
        })
    }
}

/// Discover and authenticate the identity encoded by local Zone genesis.
pub(crate) async fn build<P>(
    provider: &P,
    l1: &DynProvider<TempoNetwork>,
    config: &CheckerConfig,
) -> Result<Bootstrap, AttemptError>
where
    P: BlockNumReader + ChainSpecProvider + StateProviderFactory + ?Sized,
    P::ChainSpec: TempoHardforks,
{
    let portal = config.portal_address;
    let zone_id = config.zone_id;
    let zone_chain_id = config.zone_chain_id;
    if zone_id == 0 {
        return Err(AttemptError::disable(eyre::eyre!(
            "Zone ID must not be zero"
        )));
    }
    let l1_chain_id = l1.get_chain_id().await.map_err(classify_rpc_error)?;
    let expected_chain_id = zone_primitives::constants::zone_chain_id(l1_chain_id, zone_id)
        .map_err(AttemptError::disable)?;
    if zone_chain_id != expected_chain_id {
        return Err(AttemptError::disable(eyre::eyre!(
            "Zone chain ID {zone_chain_id} does not match Zone {zone_id} on Tempo {l1_chain_id}"
        )));
    }

    let zone_hash = provider
        .block_hash(GENESIS_BLOCK)
        .map_err(AttemptError::disable)?
        .ok_or_else(|| AttemptError::disable(eyre::eyre!("local Zone genesis is unavailable")))?;
    let (tempo, initial_token) =
        read_genesis(provider, zone_hash).map_err(AttemptError::disable)?;
    let creation = discover_creation(l1, config, initial_token, tempo.number).await?;
    if !is_canonical(l1, creation, "Portal creation").await? {
        return Err(AttemptError::retry(eyre::eyre!(
            "Portal creation changed during bootstrap"
        )));
    }
    if !is_canonical(l1, tempo, "Zone genesis anchor").await? {
        return Err(AttemptError::disable(eyre::eyre!(
            "Zone genesis Tempo anchor is not canonical"
        )));
    }

    Ok(Bootstrap {
        identity: persistence::Identity {
            l1_chain_id,
            zone_chain_id,
            zone_id,
            portal,
            creation: creation.into(),
        },
        zone: persistence::BlockRef::new(GENESIS_BLOCK, zone_hash),
        tempo: persistence::BlockRef::from(tempo),
        initial_token,
    })
}

/// Read and validate Zone genesis, returning its Tempo anchor and initial token.
fn read_genesis<P>(provider: &P, hash: B256) -> eyre::Result<(BlockNumHash, Address)>
where
    P: ChainSpecProvider + StateProviderFactory + ?Sized,
    P::ChainSpec: TempoHardforks,
{
    let chain_spec = provider.chain_spec();
    let spec = chain_spec.tempo_hardfork_at(chain_spec.genesis_header().timestamp());
    let snapshot = read_zone_genesis(provider, hash, spec)?;
    eyre::ensure!(
        !snapshot.tempo_block_hash.is_zero(),
        "Zone genesis has no Tempo anchor"
    );
    eyre::ensure!(
        snapshot.processed_deposit_queue_hash.is_zero()
            && snapshot.processed_deposit_number == 0
            && snapshot.withdrawal_queue_hash.is_zero()
            && snapshot.withdrawal_batch_index == 0,
        "Zone genesis contains prior bridge progress"
    );
    eyre::ensure!(
        !snapshot.default_fee_token.is_zero(),
        "Zone genesis has no initial token"
    );
    eyre::ensure!(
        snapshot.initial_token_supply.is_zero(),
        "Zone genesis initial token has nonzero supply"
    );
    Ok((
        BlockNumHash::new(snapshot.tempo_block_number, snapshot.tempo_block_hash),
        snapshot.default_fee_token,
    ))
}

/// Use the RPC log index for discovery, then authenticate the matching receipt.
async fn discover_creation(
    provider: &DynProvider<TempoNetwork>,
    config: &CheckerConfig,
    initial_token: Address,
    anchor_number: u64,
) -> Result<BlockNumHash, AttemptError> {
    let head = provider
        .get_block_number()
        .await
        .map_err(classify_rpc_error)?;
    let filters = (0..=head).step_by(LOG_QUERY_BLOCKS as usize).map(|start| {
        Filter::new()
            .address(ZONE_FACTORY_ADDRESS)
            .event_signature(ZoneFactory::ZoneCreated::SIGNATURE_HASH)
            .topic1(B256::from(U256::from(config.zone_id)))
            .topic2(config.portal_address.into_word())
            .from_block(start)
            .to_block(start.saturating_add(LOG_QUERY_BLOCKS - 1).min(head))
    });
    let pages = stream::iter(filters)
        .map(|filter| async move { provider.get_logs(&filter).await.map_err(classify_rpc_error) })
        .buffer_unordered(LOG_QUERY_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    let mut candidates = Vec::new();
    for page in pages {
        for log in page {
            if !log.removed {
                candidates.push(log.block_hash.ok_or_else(|| {
                    AttemptError::disable(eyre::eyre!(
                        "ZoneCreated discovery result has no block hash"
                    ))
                })?);
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    let hash = match candidates.as_slice() {
        [hash] => *hash,
        [] => {
            return Err(AttemptError::retry(eyre::eyre!(
                "creation block is not yet available for Zone {} and Portal {} after genesis anchor {}",
                config.zone_id,
                config.portal_address,
                anchor_number,
            )));
        }
        _ => {
            return Err(AttemptError::disable(eyre::eyre!(
                "expected one creation block for Zone {} and Portal {}, found {}",
                config.zone_id,
                config.portal_address,
                candidates.len()
            )));
        }
    };
    authenticate_creation(provider, hash, config, initial_token).await
}

/// Require one matching `ZoneCreated` event with authenticated receipt provenance.
async fn authenticate_creation(
    provider: &DynProvider<TempoNetwork>,
    hash: B256,
    config: &CheckerConfig,
    initial_token: Address,
) -> Result<BlockNumHash, AttemptError> {
    let block = provider
        .get_block_by_hash(hash)
        .hashes()
        .await
        .map_err(classify_rpc_error)?
        .ok_or_else(|| {
            AttemptError::retry(eyre::eyre!("Portal creation block {hash} is unavailable"))
        })?;
    let coordinate = BlockNumHash::new(block.header().number(), hash);
    let receipts = provider
        .get_block_receipts(BlockId::hash(hash))
        .await
        .map_err(classify_rpc_error)?
        .ok_or_else(|| {
            AttemptError::retry(eyre::eyre!("Portal creation receipts are unavailable"))
        })?;
    zone_l1::verify_receipts_against_header(
        coordinate,
        block.header().receipts_root(),
        block.header().logs_bloom(),
        &receipts,
    )
    .map_err(AttemptError::disable)?;

    let mut creations = receipts
        .iter()
        .filter(|receipt| receipt.status())
        .flat_map(|receipt| receipt.logs())
        .filter(|log| log.address() == ZONE_FACTORY_ADDRESS)
        .filter(|log| log.topic0() == Some(&ZoneFactory::ZoneCreated::SIGNATURE_HASH))
        .map(|log| {
            decode_event::<ZoneFactory::ZoneCreated>(&log.inner, "ZoneCreated", coordinate.number)
        })
        .collect::<eyre::Result<Vec<_>>>()
        .map_err(AttemptError::disable)?
        .into_iter()
        .filter(|event| event.zoneId == config.zone_id && event.portal == config.portal_address);
    let event = creations.next().ok_or_else(|| {
        AttemptError::disable(eyre::eyre!(
            "authenticated block has no matching ZoneCreated event"
        ))
    })?;
    if creations.next().is_some() {
        return Err(AttemptError::disable(eyre::eyre!(
            "authenticated block has multiple matching ZoneCreated events"
        )));
    }
    if event.initialToken != initial_token {
        return Err(AttemptError::disable(eyre::eyre!(
            "Portal initial token does not match Zone genesis"
        )));
    }
    Ok(coordinate)
}

async fn is_canonical(
    provider: &DynProvider<TempoNetwork>,
    coordinate: BlockNumHash,
    name: &str,
) -> Result<bool, AttemptError> {
    let block = provider
        .get_block_by_number(coordinate.number.into())
        .await
        .map_err(classify_rpc_error)?
        .ok_or_else(|| AttemptError::retry(eyre::eyre!("canonical {name} block is unavailable")))?;
    Ok(block.header().hash == coordinate.hash)
}

async fn initial_state(
    provider: &DynProvider<TempoNetwork>,
    config: &CheckerConfig,
    creation: BlockNumHash,
    anchor: BlockNumHash,
    initial_token: Address,
) -> Result<State, AttemptError> {
    let mut state = State::default();
    if anchor.number < creation.number {
        return Ok(state);
    }
    let block = provider
        .get_block_by_hash(creation.hash)
        .await
        .map_err(classify_rpc_error)?
        .ok_or_else(|| AttemptError::retry(eyre::eyre!("Portal creation block is unavailable")))?;
    let parent = BlockNumHash::new(
        creation.number.checked_sub(1).ok_or_else(|| {
            AttemptError::disable(eyre::eyre!("Portal cannot be created in Tempo genesis"))
        })?,
        block.header().parent_hash(),
    );
    let mut previous = parent;
    while previous.number < anchor.number {
        let block = collect_l1_block(provider, config.portal_address, previous).await?;
        state
            .apply(&effects::from_tempo(&block))
            .map_err(AttemptError::disable)?;
        previous = block.block();
    }
    if previous != anchor {
        return Err(AttemptError::disable(eyre::eyre!(
            "Tempo history does not end at the Zone genesis anchor"
        )));
    }
    if state.token(initial_token).is_none() {
        return Err(AttemptError::disable(eyre::eyre!(
            "Portal creation did not enable the Zone genesis token"
        )));
    }
    let tokens = state.tokens().map(|(token, _)| token).collect::<Vec<_>>();
    let balances = portal_balances(provider, config.portal_address, tokens, anchor.hash).await?;
    state
        .verify_portal_balances(balances)
        .map_err(AttemptError::disable)?;
    Ok(state)
}
