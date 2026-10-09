//! Checks that the sequencer's fee recipient may receive every Zone fee token.
//!
//! Fee collection requires the block beneficiary to be an authorized TIP-403 recipient of the
//! fee token, and an unauthorized beneficiary makes every transaction paying in that token
//! invalid. Zone base fees are zero before T14 and zero-fee transactions skip fee collection, so
//! T14 is the first fork where the requirement is routinely exercised.
//!
//! The check is diagnostic only: it runs when a sequencer engine starts and logs every token that
//! rejects the recipient. It never blocks block production, because the token policy lives on L1
//! and can change after the check passes.

use std::{collections::BTreeSet, fmt, sync::Arc};

use alloy_consensus::BlockHeader as _;
use alloy_primitives::Address;
use alloy_rpc_types_eth::{TransactionRequest, state::EvmOverrides};
use alloy_sol_types::SolCall;
use eyre::WrapErr as _;
use reth_provider::ChainSpecProvider;
use reth_rpc_eth_api::{
    EthApiTypes,
    helpers::{EthCall, FullEthApi},
};
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionRequest};
use tempo_chainspec::spec::TempoHardforks as _;
use tempo_contracts::precompiles::{ITIP20, ITIP403Registry};
use tempo_precompiles::TIP403_REGISTRY_ADDRESS;
use tracing::{error, info, warn};
use zone_chainspec::ZoneChainSpec;
use zone_l1::state::EnabledTokenRegistry;
use zone_precompiles::{ZONE_FEE_MANAGER_ADDRESS, zone_fee_manager};

/// Returns the fee tokens whose TIP-403 transfer policy rejects `recipient` as a recipient.
///
/// Evaluates the same policy lookup as fee collection against the latest Zone state, including
/// the finalized L1 policy overlay.
pub async fn unauthorized_fee_tokens<Api>(
    api: &Api,
    tokens: impl IntoIterator<Item = Address>,
    recipient: Address,
) -> eyre::Result<Vec<Address>>
where
    Api: EthCall + EthApiTypes<NetworkTypes = TempoNetwork>,
{
    let mut unauthorized = Vec::new();
    for token in tokens {
        let policy_id = call(api, token, ITIP20::transferPolicyIdCall {})
            .await
            .wrap_err_with(|| format!("failed to read transfer policy of fee token {token}"))?;
        let authorized = call(
            api,
            TIP403_REGISTRY_ADDRESS,
            ITIP403Registry::isAuthorizedRecipientCall {
                policyId: policy_id,
                user: recipient,
            },
        )
        .await
        .wrap_err_with(|| format!("failed to check TIP-403 policy {policy_id} of {token}"))?;
        if !authorized {
            unauthorized.push(token);
        }
    }
    Ok(unauthorized)
}

/// Runs an operator `eth_call` from the zero address, which the TIP-403 registry admits.
async fn call<Api, C>(api: &Api, to: Address, call: C) -> eyre::Result<C::Return>
where
    Api: EthCall + EthApiTypes<NetworkTypes = TempoNetwork>,
    C: SolCall,
{
    let request = TempoTransactionRequest {
        inner: TransactionRequest {
            from: Some(Address::ZERO),
            to: Some(to.into()),
            input: call.abi_encode().into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let output = EthCall::call(api, request, None, EvmOverrides::default())
        .await
        .map_err(|error| eyre::eyre!(error.to_string()))?;
    C::abi_decode_returns(output.as_ref()).map_err(Into::into)
}

/// Spawns fee-recipient checks for newly started sequencer engines.
#[derive(Clone)]
pub(crate) struct FeeRecipientCheck(Arc<dyn Fn(Address) + Send + Sync>);

impl FeeRecipientCheck {
    pub(crate) fn new<Api>(api: Api, enabled_tokens: EnabledTokenRegistry) -> Self
    where
        Api: FullEthApi + EthApiTypes<NetworkTypes = TempoNetwork> + Clone + Send + Sync + 'static,
        Api::Provider: ChainSpecProvider<ChainSpec = ZoneChainSpec>,
    {
        Self(Arc::new(move |recipient| {
            let api = api.clone();
            let enabled_tokens = enabled_tokens.clone();
            tokio::spawn(async move {
                if let Err(err) = check_fee_recipient(&api, &enabled_tokens, recipient).await {
                    warn!(
                        target: "zone::fees",
                        %recipient,
                        %err,
                        "Failed to check that the fee recipient can receive Zone fee tokens"
                    );
                }
            });
        }))
    }

    /// Checks `recipient` in the background and logs the outcome.
    pub(crate) fn spawn(&self, recipient: Address) {
        (self.0)(recipient)
    }
}

impl fmt::Debug for FeeRecipientCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FeeRecipientCheck").finish_non_exhaustive()
    }
}

/// Checks the enabled tokens and the default fee token, logging at error level once T14 is
/// active and at warn level before it.
async fn check_fee_recipient<Api>(
    api: &Api,
    enabled_tokens: &EnabledTokenRegistry,
    recipient: Address,
) -> eyre::Result<()>
where
    Api: FullEthApi + EthApiTypes<NetworkTypes = TempoNetwork>,
    Api::Provider: ChainSpecProvider<ChainSpec = ZoneChainSpec>,
{
    let provider = api.provider();
    let latest = provider
        .latest_header()?
        .ok_or_else(|| eyre::eyre!("latest Zone block not found"))?;
    let default_fee_token = Address::from_word(
        provider
            .latest()?
            .storage(
                ZONE_FEE_MANAGER_ADDRESS,
                zone_fee_manager::slots::DEFAULT_FEE_TOKEN.into(),
            )?
            .unwrap_or_default()
            .into(),
    );
    let mut tokens: BTreeSet<Address> = enabled_tokens.read().iter().copied().collect();
    if !default_fee_token.is_zero() {
        tokens.insert(default_fee_token);
    }

    let unauthorized = unauthorized_fee_tokens(api, tokens.iter().copied(), recipient).await?;
    if unauthorized.is_empty() {
        info!(
            target: "zone::fees",
            %recipient,
            tokens = tokens.len(),
            "Fee recipient is an authorized TIP-403 recipient of every Zone fee token"
        );
    } else if provider
        .chain_spec()
        .tempo_hardfork_at(latest.timestamp())
        .is_t14()
    {
        error!(
            target: "zone::fees",
            %recipient,
            ?unauthorized,
            "Fee recipient is not an authorized TIP-403 recipient of these fee tokens; \
             transactions paying fees in them are invalid"
        );
    } else {
        warn!(
            target: "zone::fees",
            %recipient,
            ?unauthorized,
            "Fee recipient is not an authorized TIP-403 recipient of these fee tokens; \
             transactions paying nonzero fees in them are invalid, including every transaction \
             once T14 activates"
        );
    }
    Ok(())
}
