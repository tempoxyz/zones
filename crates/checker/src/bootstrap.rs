//! Authenticates Zone genesis and discovers its Portal creation on Tempo.

use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockId, BlockNumHash};
use alloy_network::{BlockResponse as _, ReceiptResponse as _};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_rpc_types_eth::Filter;
use alloy_sol_types::SolEvent as _;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_storage_api::{BlockNumReader, StateProviderFactory};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TempoHardforks;
use tempo_zone_contracts::{ZONE_FACTORY_ADDRESS, ZoneFactory};

use crate::{
    AttemptError, CheckerConfig,
    accounting::{State, effects},
    decode_event,
    l1::{
        L1ReadError, classify_rpc_error, collect_l1_block, portal_balances, validate_l1_receipts,
    },
    l2::read_zone_genesis,
    persistence::{self, Checkpoint},
};

const GENESIS_BLOCK: u64 = 0;
const LOG_QUERY_BLOCKS: u64 = 10_000;

/// Discover and authenticate the checkpoint encoded by local Zone genesis.
pub(crate) async fn build<P>(
    provider: &P,
    l1: &DynProvider<TempoNetwork>,
    config: &CheckerConfig,
) -> Result<Checkpoint, AttemptError>
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
    let creation = discover_creation(l1, portal, zone_id, initial_token, tempo.number).await?;
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

    let state = initial_state(l1, portal, creation, tempo, initial_token).await?;
    Ok(Checkpoint {
        identity: persistence::Identity {
            l1_chain_id,
            zone_chain_id,
            zone_id,
            portal,
            creation: creation.into(),
        },
        zone: persistence::BlockRef::new(GENESIS_BLOCK, zone_hash),
        tempo: persistence::BlockRef::from(tempo),
        state,
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
    portal: Address,
    zone_id: u32,
    initial_token: Address,
    anchor_number: u64,
) -> Result<BlockNumHash, AttemptError> {
    let head = provider
        .get_block_number()
        .await
        .map_err(classify_rpc_error)?;
    let mut candidates = Vec::new();
    let mut start = 0;
    while start <= head {
        let end = start.saturating_add(LOG_QUERY_BLOCKS - 1).min(head);
        let filter = Filter::new()
            .address(ZONE_FACTORY_ADDRESS)
            .event_signature(ZoneFactory::ZoneCreated::SIGNATURE_HASH)
            .topic1(B256::from(U256::from(zone_id)))
            .topic2(portal.into_word())
            .from_block(start)
            .to_block(end);
        for log in provider
            .get_logs(&filter)
            .await
            .map_err(classify_rpc_error)?
        {
            if !log.removed {
                candidates.push(log.block_hash.ok_or_else(|| {
                    AttemptError::disable(eyre::eyre!(
                        "ZoneCreated discovery result has no block hash"
                    ))
                })?);
            }
        }
        start = match end.checked_add(1) {
            Some(next) => next,
            None => break,
        };
    }
    candidates.sort_unstable();
    candidates.dedup();
    let hash = match candidates.as_slice() {
        [hash] => *hash,
        [] if head < anchor_number => {
            return Err(AttemptError::retry(eyre::eyre!(
                "Tempo has not reached Zone genesis anchor {anchor_number}"
            )));
        }
        [] => {
            return Err(AttemptError::disable(eyre::eyre!(
                "no creation block found for Zone {zone_id} and Portal {portal}"
            )));
        }
        _ => {
            return Err(AttemptError::disable(eyre::eyre!(
                "expected one creation block for Zone {} and Portal {}, found {}",
                zone_id,
                portal,
                candidates.len()
            )));
        }
    };
    authenticate_creation(provider, hash, portal, zone_id, initial_token).await
}

/// Require one matching `ZoneCreated` event with authenticated receipt provenance.
async fn authenticate_creation(
    provider: &DynProvider<TempoNetwork>,
    hash: B256,
    portal: Address,
    zone_id: u32,
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
    let transaction_hashes = block.transactions().hashes().collect::<Vec<_>>();
    validate_l1_receipts(coordinate, &transaction_hashes, &receipts)
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
        .filter(|event| event.zoneId == zone_id && event.portal == portal);
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
    portal: Address,
    creation: BlockNumHash,
    anchor: BlockNumHash,
    initial_token: Address,
) -> Result<State, AttemptError> {
    let mut state = State::default();
    state
        .apply(&[crate::accounting::Effect::EnableToken(initial_token)])
        .map_err(AttemptError::disable)?;
    if anchor.number < creation.number {
        return Err(AttemptError::disable(eyre::eyre!(
            "Zone genesis anchor predates Portal creation"
        )));
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
        let block = collect_l1_block(provider, portal, previous)
            .await
            .map_err(classify_bootstrap_l1_error)?;
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
    let tokens = state.tokens().map(|(token, _)| token).collect::<Vec<_>>();
    let balances = portal_balances(provider, portal, tokens, anchor.hash)
        .await
        .map_err(classify_bootstrap_l1_error)?;
    state
        .verify_portal_balances(balances)
        .map_err(AttemptError::disable)?;
    Ok(state)
}

fn classify_bootstrap_l1_error(error: L1ReadError) -> AttemptError {
    match error {
        L1ReadError::Unavailable(error) => AttemptError::Retry(error),
        L1ReadError::Finding(error) | L1ReadError::Disable(error) => AttemptError::Disable(error),
    }
}
