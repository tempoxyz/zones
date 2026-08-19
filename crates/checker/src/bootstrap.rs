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
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionReceipt};
use tempo_chainspec::spec::TempoHardforks;
use tempo_zone_contracts::{ZONE_FACTORY_ADDRESS, ZoneFactory};

use crate::{
    CheckerConfig,
    accounting::{State, effects},
    decode_event,
    l1::{collect_l1_block, portal_balances},
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
) -> eyre::Result<Checkpoint>
where
    P: BlockNumReader + ChainSpecProvider + StateProviderFactory + ?Sized,
    P::ChainSpec: TempoHardforks,
{
    let portal = config.portal_address;
    let zone_id = config.zone_id;
    let zone_chain_id = config.zone_chain_id;
    eyre::ensure!(zone_id != 0, "Zone ID must not be zero");
    let l1_chain_id = l1.get_chain_id().await?;
    let expected_chain_id = zone_primitives::constants::zone_chain_id(l1_chain_id, zone_id)?;
    eyre::ensure!(
        zone_chain_id == expected_chain_id,
        "Zone chain ID {zone_chain_id} does not match Zone {zone_id} on Tempo {l1_chain_id}"
    );

    let zone_hash = provider
        .block_hash(GENESIS_BLOCK)?
        .ok_or_else(|| eyre::eyre!("local Zone genesis is unavailable"))?;
    let (tempo, initial_token) = read_genesis(provider, zone_hash)?;
    let creation = discover_creation(l1, portal, zone_id, initial_token).await?;
    ensure_canonical(l1, creation, "Portal creation").await?;
    ensure_canonical(l1, tempo, "Zone genesis anchor").await?;

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
) -> eyre::Result<BlockNumHash> {
    let head = provider.get_block_number().await?;
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
        for log in provider.get_logs(&filter).await? {
            if !log.removed {
                candidates.push(log.block_hash.ok_or_else(|| {
                    eyre::eyre!("ZoneCreated discovery result has no block hash")
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
    let [hash] = candidates.as_slice() else {
        eyre::bail!(
            "expected one creation block for Zone {zone_id} and Portal {portal}, found {}",
            candidates.len()
        );
    };
    authenticate_creation(provider, *hash, portal, zone_id, initial_token).await
}

/// Require one matching `ZoneCreated` event with authenticated receipt provenance.
async fn authenticate_creation(
    provider: &DynProvider<TempoNetwork>,
    hash: B256,
    portal: Address,
    zone_id: u32,
    initial_token: Address,
) -> eyre::Result<BlockNumHash> {
    let block = provider
        .get_block_by_hash(hash)
        .hashes()
        .await?
        .ok_or_else(|| eyre::eyre!("Portal creation block {hash} is unavailable"))?;
    let number = block.header().number();
    let receipts = provider
        .get_block_receipts(BlockId::hash(hash))
        .await?
        .ok_or_else(|| eyre::eyre!("Portal creation receipts are unavailable"))?;
    let transaction_hashes = block.transactions().hashes().collect::<Vec<_>>();
    authenticate_receipts(hash, number, &transaction_hashes, &receipts)?;

    let mut creations = receipts
        .iter()
        .filter(|receipt| receipt.status())
        .flat_map(|receipt| receipt.logs())
        .filter(|log| log.address() == ZONE_FACTORY_ADDRESS)
        .filter(|log| log.topic0() == Some(&ZoneFactory::ZoneCreated::SIGNATURE_HASH))
        .map(|log| decode_event::<ZoneFactory::ZoneCreated>(&log.inner, "ZoneCreated", number))
        .collect::<eyre::Result<Vec<_>>>()?
        .into_iter()
        .filter(|event| event.zoneId == zone_id && event.portal == portal);
    let event = creations
        .next()
        .ok_or_else(|| eyre::eyre!("authenticated block has no matching ZoneCreated event"))?;
    eyre::ensure!(
        creations.next().is_none(),
        "authenticated block has multiple matching ZoneCreated events"
    );
    eyre::ensure!(
        event.initialToken == initial_token,
        "Portal initial token does not match Zone genesis"
    );
    Ok(BlockNumHash::new(number, hash))
}

/// Require each receipt's provenance to match the block it was fetched from.
fn authenticate_receipts(
    hash: B256,
    number: u64,
    transaction_hashes: &[B256],
    receipts: &[TempoTransactionReceipt],
) -> eyre::Result<()> {
    eyre::ensure!(
        transaction_hashes.len() == receipts.len(),
        "creation block transaction and receipt counts differ"
    );
    for (index, (transaction, receipt)) in transaction_hashes.iter().zip(receipts).enumerate() {
        eyre::ensure!(
            receipt.block_hash() == Some(hash)
                && receipt.block_number() == Some(number)
                && receipt.transaction_hash() == *transaction
                && receipt.transaction_index() == Some(index as u64),
            "creation receipt {index} has inconsistent provenance"
        );
    }
    Ok(())
}

async fn ensure_canonical(
    provider: &DynProvider<TempoNetwork>,
    coordinate: BlockNumHash,
    name: &str,
) -> eyre::Result<()> {
    let block = provider
        .get_block_by_number(coordinate.number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("canonical {name} block is unavailable"))?;
    eyre::ensure!(
        block.header().hash == coordinate.hash,
        "{name} block changed during bootstrap"
    );
    Ok(())
}

async fn initial_state(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    creation: BlockNumHash,
    anchor: BlockNumHash,
    initial_token: Address,
) -> eyre::Result<State> {
    let mut state = State::default();
    state.apply(&[crate::accounting::Effect::EnableToken {
        token: initial_token,
    }])?;
    if anchor.number < creation.number {
        return Ok(state);
    }
    let block = provider
        .get_block_by_hash(creation.hash)
        .await?
        .ok_or_else(|| eyre::eyre!("Portal creation block is unavailable"))?;
    let parent = BlockNumHash::new(
        creation
            .number
            .checked_sub(1)
            .ok_or_else(|| eyre::eyre!("Portal cannot be created in Tempo genesis"))?,
        block.header().parent_hash(),
    );
    let mut previous = parent;
    while previous.number < anchor.number {
        let block = collect_l1_block(provider, portal, previous).await?;
        state.apply(&effects::from_tempo(&block))?;
        previous = block.block();
    }
    eyre::ensure!(
        previous == anchor,
        "Tempo history does not end at the Zone genesis anchor"
    );
    let tokens = state.tokens().map(|(token, _)| token).collect::<Vec<_>>();
    let balances = portal_balances(provider, portal, tokens, anchor.hash).await?;
    state.verify_portal_balances(balances)?;
    Ok(state)
}
