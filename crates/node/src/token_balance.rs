use alloy_primitives::Address;
use eyre::WrapErr;
use reth_storage_api::{StateProvider, StateProviderFactory};
use tempo_precompiles::tip20::TIP20Token;

/// Returns whether `account` holds a nonzero balance of any enabled Zone token.
pub(crate) fn has_enabled_token_balance(
    provider: &impl StateProviderFactory,
    enabled_tokens: impl IntoIterator<Item = Address>,
    account: Address,
) -> eyre::Result<bool> {
    let state = provider
        .latest()
        .wrap_err("failed to read latest state for Zone token-balance check")?;

    for token in enabled_tokens {
        let slot = TIP20Token::from_address_unchecked(token).balances[account].slot();
        let balance = state
            .storage(token, slot.into())
            .wrap_err("failed to read Zone token balance")?;
        if balance.is_some_and(|balance| !balance.is_zero()) {
            return Ok(true);
        }
    }

    Ok(false)
}
