//! Hand-written helpers for the [`ZonePortal`](crate::bindings::ZonePortal) bindings.

use crate::bindings::ZonePortal;

impl ZonePortal::sequencerEncryptionKeyReturn {
    /// Normalize `yParity` to SEC1 compressed prefix (`0x02` or `0x03`).
    ///
    /// The contract may return `0`/`1` (parity bit) or `0x02`/`0x03` (SEC1 prefix).
    pub fn normalized_y_parity(&self) -> Option<u8> {
        match self.yParity {
            0x02 | 0x03 => Some(self.yParity),
            0 | 1 => Some(0x02 + self.yParity),
            _ => None,
        }
    }
}

impl core::fmt::Display for ZonePortal::ZonePortalErrors {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotSequencer(_) => f.write_str("NotSequencer"),
            Self::InvalidProof(_) => f.write_str("InvalidProof"),
            Self::InvalidTempoBlockNumber(_) => f.write_str("InvalidTempoBlockNumber"),
            Self::DepositPolicyForbids(_) => f.write_str("DepositPolicyForbids"),
        }
    }
}

#[cfg(feature = "rpc")]
impl<P: alloy_provider::Provider<N>, N: alloy_network::Network>
    ZonePortal::ZonePortalInstance<P, N>
{
    /// Returns all token addresses currently enabled for bridging on this [`ZonePortal`].
    ///
    /// Calls [`enabledTokenCount`](ZonePortal::enabledTokenCountCall) followed by
    /// [`enabledTokenAt`](ZonePortal::enabledTokenAtCall) for each index concurrently.
    pub async fn enabled_tokens(
        &self,
    ) -> Result<alloc::vec::Vec<alloy_primitives::Address>, alloy_contract::Error> {
        let count = self.enabledTokenCount().call().await?;
        let futs: alloc::vec::Vec<_> = (0..count.to::<u64>())
            .map(|i| async move {
                self.enabledTokenAt(alloy_primitives::U256::from(i))
                    .call()
                    .await
            })
            .collect();
        futures::future::try_join_all(futs).await
    }

    /// Fetches the active sequencer encryption key and its index.
    ///
    /// Returns `(key, key_index)` where `key` is the
    /// [`sequencerEncryptionKeyReturn`](ZonePortal::sequencerEncryptionKeyReturn) and
    /// `key_index` is the zero-based index of the current key.
    pub async fn encryption_key(
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
