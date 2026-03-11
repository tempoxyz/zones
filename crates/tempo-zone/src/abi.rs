//! ABI bindings — re-exports from [`zone_primitives::abi`] plus RPC extension methods.

pub use zone_primitives::abi::*;

/// Extension methods for [`ZonePortal::ZonePortalInstance`] that require an async provider.
pub trait ZonePortalExt {
    /// Returns all token addresses currently enabled for bridging on this [`ZonePortal`].
    fn enabled_tokens(
        &self,
    ) -> impl Future<Output = Result<Vec<alloy_primitives::Address>, alloy_contract::Error>>;

    /// Fetches the active sequencer encryption key and its index.
    ///
    /// Returns `(key, key_index)` where `key` is the
    /// [`sequencerEncryptionKeyReturn`](ZonePortal::sequencerEncryptionKeyReturn) and
    /// `key_index` is the zero-based index of the current key.
    fn encryption_key(
        &self,
    ) -> impl Future<
        Output = Result<
            (
                ZonePortal::sequencerEncryptionKeyReturn,
                alloy_primitives::U256,
            ),
            alloy_contract::Error,
        >,
    >;
}

use std::future::Future;

impl<P: alloy_provider::Provider<N>, N: alloy_network::Network> ZonePortalExt
    for ZonePortal::ZonePortalInstance<P, N>
{
    async fn enabled_tokens(
        &self,
    ) -> Result<Vec<alloy_primitives::Address>, alloy_contract::Error> {
        let count = self.enabledTokenCount().call().await?;
        let mut tokens = Vec::with_capacity(count.to::<usize>());
        for i in 0..count.to::<u64>() {
            let token = self
                .enabledTokenAt(alloy_primitives::U256::from(i))
                .call()
                .await?;
            tokens.push(token);
        }
        Ok(tokens)
    }

    async fn encryption_key(
        &self,
    ) -> Result<
        (
            ZonePortal::sequencerEncryptionKeyReturn,
            alloy_primitives::U256,
        ),
        alloy_contract::Error,
    > {
        let key_call = self.sequencerEncryptionKey();
        let count_call = self.encryptionKeyCount();
        let (key, count) = tokio::try_join!(key_call.call(), count_call.call())?;
        let key_index = count.saturating_sub(alloy_primitives::U256::from(1));
        Ok((key, key_index))
    }
}
