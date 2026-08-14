//! Discovers and authenticates the ZoneFactory creation block for bootstrap.

use alloy_primitives::{B256, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types_eth::Filter;
use alloy_sol_types::SolEvent;
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::{ZONE_FACTORY_ADDRESS, ZoneFactory};

use crate::{
    adapter::adapt_imported,
    kernel::{ImportedOperation, PortalIdentity},
    observe::{ImportedTempoHeader, L1BlockObservation, acquire_l1_header, observe_l1},
};

use super::BootstrapError;

const LOG_QUERY_BLOCKS: u64 = 10_000;

/// Authenticated creation data reused when bootstrap replays the creation block.
pub(super) struct Creation {
    pub(super) header: ImportedTempoHeader,
    pub(super) observation: L1BlockObservation,
}

/// Locate the unique ZoneFactory event matching Zone genesis and authenticate its block.
pub(super) async fn discover_creation(
    provider: &DynProvider<TempoNetwork>,
    expected: PortalIdentity,
) -> eyre::Result<Creation> {
    let head = provider.get_block_number().await?;
    let candidates = creation_candidates(provider, expected, 0, head).await?;
    let [candidate] = candidates.as_slice() else {
        return Err(BootstrapError::CreationCandidates {
            portal: expected.portal,
            zone_id: expected.zone_id,
            count: candidates.len(),
        }
        .into());
    };
    let header = acquire_l1_header(provider, *candidate).await?;
    let observation = observe_l1(provider, &header, expected.portal).await?;
    let facts = adapt_imported(&observation, &header, header.hash(), expected.zone_id)
        .map_err(|failure| eyre::eyre!(failure.message))?
        .facts;
    validate_creation(&facts.operations, expected)?;
    Ok(Creation {
        header,
        observation,
    })
}

/// Find canonical factory-log candidates in one inclusive Tempo block range.
async fn creation_candidates(
    provider: &DynProvider<TempoNetwork>,
    expected: PortalIdentity,
    from: u64,
    to: u64,
) -> eyre::Result<Vec<B256>> {
    let mut hashes = Vec::new();
    let mut start = from;
    while start <= to {
        let end = start.saturating_add(LOG_QUERY_BLOCKS - 1).min(to);
        let filter = Filter::new()
            .address(ZONE_FACTORY_ADDRESS)
            .event_signature(ZoneFactory::ZoneCreated::SIGNATURE_HASH)
            .topic1(B256::from(U256::from(expected.zone_id)))
            .topic2(expected.portal.into_word())
            .from_block(start)
            .to_block(end);
        hashes.extend(
            provider
                .get_logs(&filter)
                .await?
                .into_iter()
                .filter(|log| !log.removed)
                .map(|log| {
                    log.block_hash.ok_or_else(|| {
                        eyre::eyre!("ZoneCreated discovery log is missing its canonical block hash")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        if end == u64::MAX {
            break;
        }
        start = end + 1;
    }
    Ok(hashes)
}

/// Validate the authenticated Portal creation operation against Zone genesis.
fn validate_creation(
    operations: &[ImportedOperation],
    expected: PortalIdentity,
) -> eyre::Result<()> {
    let mut creations = operations.iter().filter_map(|operation| match operation {
        ImportedOperation::Create {
            identity,
            initial_token,
        } => Some((identity, initial_token)),
        _ => None,
    });
    let Some((identity, initial_token)) = creations.next() else {
        eyre::bail!("creation block is missing the portal creation operation");
    };
    if creations.next().is_some() {
        eyre::bail!("creation block contains multiple portal creation operations");
    }
    if *identity != expected || initial_token.token != expected.initial_token {
        eyre::bail!("creation identity does not match Zone genesis");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;
    use crate::kernel::TokenEnable;

    fn identity() -> PortalIdentity {
        PortalIdentity {
            portal: Address::repeat_byte(1),
            zone_id: 7,
            initial_token: Address::repeat_byte(2),
        }
    }

    fn creation(identity: PortalIdentity) -> ImportedOperation {
        ImportedOperation::Create {
            initial_token: TokenEnable {
                token: identity.initial_token,
                name: String::new(),
                symbol: String::new(),
                currency: String::new(),
            },
            identity,
        }
    }

    #[test]
    fn creation_requires_one_matching_operation() {
        let expected = identity();
        assert!(validate_creation(&[creation(expected)], expected).is_ok());
        assert!(validate_creation(&[], expected).is_err());

        let mut mismatched = expected;
        mismatched.zone_id += 1;
        assert!(validate_creation(&[creation(mismatched)], expected).is_err());

        assert!(validate_creation(&[creation(expected), creation(expected)], expected).is_err());
    }

    #[test]
    fn creation_allows_later_canonical_portal_operations() {
        let expected = identity();
        let additional_token = TokenEnable {
            token: Address::repeat_byte(3),
            name: String::new(),
            symbol: String::new(),
            currency: String::new(),
        };
        let operations = [
            creation(expected),
            ImportedOperation::EnableToken(additional_token),
            ImportedOperation::UpdateBouncebackGas(42),
        ];

        assert!(validate_creation(&operations, expected).is_ok());
    }
}
