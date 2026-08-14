//! Builds the initial authenticated checker checkpoint from Zone genesis.

mod ancestry;
mod creation;
mod error;
mod zone_genesis;

use crate::kernel::{State, apply_genesis_handoff};
use alloy_eips::BlockNumHash;
use alloy_provider::{Provider as _, ProviderBuilder};
use reth_storage_api::{BlockNumReader, StateProviderFactory};
use tempo_alloy::TempoNetwork;

use crate::{
    CheckerConfig,
    adapter::adapt_imported,
    observe::observe_l1,
    persistence::{BlockNumHash as StoredBlockNumHash, ChainCut, Identity, Persistence},
};
use ancestry::{anchor_header, authenticated_path};
use creation::discover_creation;
use error::BootstrapError;
use zone_genesis::{LocalZoneIdentity, genesis_anchor, validate_zero_supply};

/// Build and atomically publish a checkpoint at local Zone genesis.
pub async fn build_checkpoint<P>(
    config: CheckerConfig,
    zone_chain_id: u64,
    zone_provider: &P,
) -> eyre::Result<()>
where
    P: BlockNumReader + StateProviderFactory + ?Sized,
{
    // Read the local Zone identity and its authenticated Tempo anchor.
    let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect(&config.l1_rpc_url)
        .await?
        .erased();
    let l1_chain_id = l1_provider.get_chain_id().await?;
    let LocalZoneIdentity {
        genesis,
        default_fee_token,
    } = LocalZoneIdentity::load(&config, l1_chain_id, zone_chain_id, zone_provider)?;
    let anchor = genesis_anchor(zone_provider, genesis)?;
    let anchor_header = anchor_header(&l1_provider, anchor).await?;
    let creation = discover_creation(&l1_provider, config.portal_address, config.zone_id).await?;
    let expected_identity = creation.identity;
    let creation_tip = BlockNumHash::new(creation.header.number(), creation.header.hash());

    // Authenticate and replay Tempo history into the Zone genesis state.
    let mut state = State::awaiting(expected_identity);
    if creation_tip.number <= anchor.number {
        for header in authenticated_path(&l1_provider, &creation.header, anchor_header).await? {
            if header.hash() == creation_tip.hash {
                imported_block(
                    &mut state,
                    &creation.observation,
                    &header,
                    creation_tip.hash,
                    config.zone_id,
                    &l1_provider,
                )
                .await?;
            } else {
                let observation = observe_l1(&l1_provider, &header, config.portal_address).await?;
                imported_block(
                    &mut state,
                    &observation,
                    &header,
                    creation_tip.hash,
                    config.zone_id,
                    &l1_provider,
                )
                .await?;
            }
        }
        validate_zero_supply(zone_provider, genesis.hash, state.tokens().map(|(t, _)| t))?;
        state.apply(&apply_genesis_handoff(&state)?)?;
    } else {
        authenticated_path(&l1_provider, &anchor_header, creation.header.clone()).await?;
        validate_zero_supply(zone_provider, genesis.hash, [default_fee_token])?;
    }

    let identity = Identity {
        l1_chain_id,
        zone_chain_id,
        zone_id: expected_identity.zone_id,
        portal: expected_identity.portal,
        creation_block: creation_tip.hash,
        creation_height: creation_tip.number,
    };
    let cut = ChainCut {
        zone: StoredBlockNumHash {
            number: genesis.number,
            hash: genesis.hash,
        },
        tempo: StoredBlockNumHash {
            number: anchor.number,
            hash: anchor.hash,
        },
    };
    ensure_canonical(&l1_provider, creation_tip, "creation").await?;
    ensure_canonical(&l1_provider, anchor, "genesis anchor").await?;
    // Persist the initial authenticated checker checkpoint.
    Persistence::create_atomic(&config.database_path, identity, cut, state)?;
    Ok(())
}

/// Confirm an authenticated Tempo coordinate still belongs to the canonical chain.
async fn ensure_canonical(
    provider: &alloy_provider::DynProvider<TempoNetwork>,
    coordinate: BlockNumHash,
    name: &str,
) -> eyre::Result<()> {
    let block = provider
        .get_block_by_number(coordinate.number.into())
        .await?
        .ok_or_else(|| {
            eyre::eyre!(
                "canonical Tempo {name} block {} is unavailable",
                coordinate.number
            )
        })?;
    eyre::ensure!(
        block.header.hash == coordinate.hash,
        "Tempo {name} block changed during checker bootstrap"
    );
    Ok(())
}

/// Apply one authenticated import and verify its effects and Portal collateral.
async fn imported_block(
    state: &mut State,
    observation: &crate::observe::L1BlockObservation,
    header: &crate::observe::ImportedTempoHeader,
    creation_block: alloy_primitives::B256,
    zone_id: u32,
    provider: &alloy_provider::DynProvider<TempoNetwork>,
) -> eyre::Result<()> {
    let adaptation = adapt_imported(observation, header, creation_block, zone_id)
        .map_err(|failure| eyre::eyre!(failure.message))?;
    let candidate = crate::kernel::apply_imported(state, &adaptation.facts)?;
    if adaptation.effects != candidate.expected_effects() {
        eyre::bail!("imported effects differ from expected effects");
    }
    for (token, accounting) in candidate.expected_accounting()? {
        let actual = crate::observe::acquire_portal_token_balance(
            provider,
            token,
            observation.portal_address(),
            observation.block_hash(),
        )
        .await?;
        if accounting
            .collateral()
            .is_none_or(|required| actual < required)
        {
            eyre::bail!("imported collateral is insufficient for token {token}");
        }
    }
    *state = candidate.into_state();
    Ok(())
}
