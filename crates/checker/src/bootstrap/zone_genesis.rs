//! Reads and validates the local Zone genesis checkpoint.

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256};
use reth_storage_api::{BlockNumReader, StateProviderFactory};

use crate::{CheckerConfig, observe::acquire_zone_post_state};

use super::BootstrapError;

const GENESIS_BLOCK: u64 = 0;

/// Local Zone genesis facts required to authenticate bootstrap history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LocalZoneIdentity {
    pub(super) genesis: BlockNumHash,
    pub(super) default_fee_token: Address,
}

impl LocalZoneIdentity {
    /// Read and validate the local Zone identity at genesis.
    pub(super) fn load<P>(
        config: &CheckerConfig,
        l1_chain_id: u64,
        zone_chain_id: u64,
        provider: &P,
    ) -> eyre::Result<Self>
    where
        P: BlockNumReader + StateProviderFactory + ?Sized,
    {
        if config.zone_id == 0 {
            return Err(BootstrapError::MissingZoneId.into());
        }
        let expected = zone_primitives::constants::zone_chain_id(l1_chain_id, config.zone_id)?;
        if zone_chain_id != expected {
            return Err(BootstrapError::ZoneChainIdMismatch {
                zone_id: config.zone_id,
                expected,
                actual: zone_chain_id,
            }
            .into());
        }
        let hash = provider
            .block_hash(GENESIS_BLOCK)
            .map_err(|source| BootstrapError::LocalCanonicalRead {
                number: GENESIS_BLOCK,
                source,
            })?
            .ok_or(BootstrapError::MissingLocalCanonical {
                number: GENESIS_BLOCK,
            })?;
        let default_fee_token = acquire_zone_post_state(provider, hash, &[])?.default_fee_token;
        if default_fee_token.is_zero() {
            return Err(BootstrapError::MissingZoneGenesisInitialToken.into());
        }
        Ok(Self {
            genesis: BlockNumHash::new(GENESIS_BLOCK, hash),
            default_fee_token,
        })
    }
}

/// Read the Tempo checkpoint embedded in Zone genesis and reject prior protocol progress.
pub(super) fn genesis_anchor<P>(provider: &P, genesis: BlockNumHash) -> eyre::Result<BlockNumHash>
where
    P: StateProviderFactory + ?Sized,
{
    let outputs = acquire_zone_post_state(provider, genesis.hash, &[])?;
    if outputs.tempo_block_hash.is_zero() {
        return Err(BootstrapError::UnsupportedBootstrapStyle.into());
    }
    if !outputs.processed_deposit_queue_hash.is_zero()
        || outputs.processed_deposit_number != 0
        || !outputs.withdrawal_queue_hash.is_zero()
        || outputs.withdrawal_batch_index != 0
    {
        return Err(BootstrapError::NonzeroZoneGenesisProgress {
            processed_deposit_queue_hash: outputs.processed_deposit_queue_hash,
            processed_deposit_number: outputs.processed_deposit_number,
            withdrawal_queue_hash: outputs.withdrawal_queue_hash,
            withdrawal_batch_index: outputs.withdrawal_batch_index,
        }
        .into());
    }
    Ok(BlockNumHash::new(
        outputs.tempo_block_number,
        outputs.tempo_block_hash,
    ))
}

/// Verify that all enabled tokens have zero supply at Zone genesis.
pub(super) fn validate_zero_supply<P>(
    provider: &P,
    genesis_hash: B256,
    tokens: impl IntoIterator<Item = Address>,
) -> eyre::Result<()>
where
    P: StateProviderFactory + ?Sized,
{
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    let outputs = acquire_zone_post_state(provider, genesis_hash, &tokens)?;
    if let Some((&token, &actual)) = outputs
        .token_supplies
        .iter()
        .find(|(_, supply)| !supply.is_zero())
    {
        return Err(BootstrapError::NonzeroZoneGenesisSupply { token, actual }.into());
    }
    Ok(())
}
