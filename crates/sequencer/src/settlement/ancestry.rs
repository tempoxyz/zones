//! Parent-linked Tempo L1 header chains for ancestry-mode batch anchors.
//!
//! Outside the EIP-2935 window, a batch proof carries RLP-encoded headers linking its block to a
//! recent anchor. [`AncestryLoader`] fetches and caches them, then resolves them into [`Ancestry`].
//! Callers choose the range and verify its base and anchor match the expected identities.

use super::*;
use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockNumHash;
use alloy_rlp::Encodable as _;
use eyre::ensure;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use parking_lot::RwLock;
use schnellru::{ByLength, LruMap};
use tempo_primitives::TempoHeader;

/// Maximum number of encoded L1 headers retained by one loader between requests.
///
/// At roughly 600 bytes per header, this caps payload storage near 150 MiB plus
/// map overhead while covering more than the current Zone E recovery gap.
const DEFAULT_CACHE_CAPACITY: u32 = 262_144;

/// Maximum number of in-flight L1 header requests for one ancestry load.
const FETCH_CONCURRENCY: usize = 16;

/// A validated, parent-linked Tempo header chain from `base` to `anchor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ancestry {
    /// Canonical identity of the block the chain starts from. Its header is not in `headers`.
    pub(crate) base: BlockNumHash,
    /// Identity of the last block in the chain. Equal to `base` when `headers` is empty.
    pub(crate) anchor: BlockNumHash,
    /// RLP-encoded headers for `base.number + 1..=anchor.number`, in ascending order.
    pub(crate) headers: Vec<Bytes>,
}

/// Bounded concurrent fetcher and cache for Tempo ancestry headers.
///
/// Consecutive batches request overlapping ranges, so each consumer keeps its own
/// loader and later requests reuse almost the entire preceding range.
pub(crate) struct AncestryLoader {
    provider: DynProvider<TempoNetwork>,
    /// Only headers from successfully resolved ranges are admitted.
    cache: RwLock<LruMap<u64, CachedHeader>>,
}

impl AncestryLoader {
    /// Create a loader with the default cache bound. The cache allocates lazily.
    pub(crate) fn new(provider: DynProvider<TempoNetwork>) -> Self {
        Self::with_capacity(provider, DEFAULT_CACHE_CAPACITY)
    }

    fn with_capacity(provider: DynProvider<TempoNetwork>, capacity: u32) -> Self {
        Self {
            provider,
            cache: RwLock::new(LruMap::new(ByLength::new(capacity))),
        }
    }

    /// Drop all cached headers and release the cache's allocation.
    pub(crate) fn clear(&self) {
        let mut cache = self.cache.write();
        if !cache.is_empty() {
            *cache = LruMap::new(ByLength::new(cache.limiter().max_length()));
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.cache.read().is_empty()
    }

    /// Load the canonical block header chain `from..=to`, fetching only the uncached ones.
    /// Validates numbering and parent-hash links before admitting headers to the cache.
    pub(crate) async fn load(&self, from: u64, to: u64) -> Result<Ancestry> {
        self.load_checked(from, to, |_| Ok(())).await
    }

    /// Load a chain, requiring the caller's identity checks before admitting fetched headers.
    pub(crate) async fn load_checked<F>(&self, from: u64, to: u64, check: F) -> Result<Ancestry>
    where
        F: FnOnce(&Ancestry) -> Result<()>,
    {
        // Snapshot the cache without changing its LRU order. Network requests
        // and validation happen after the read lock is released.
        let (cached, missing) = {
            let cache = self.cache.read();
            let mut cached = Vec::new();
            let mut missing = Vec::new();
            for number in from..=to {
                match cache.peek(&number) {
                    Some(header) => cached.push(header.clone()),
                    None => missing.push(number),
                }
            }
            (cached, missing)
        };
        let cache_hits = cached.len();

        let fetched = stream::iter(missing)
            .map(|number| self.fetch(number))
            .buffer_unordered(FETCH_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;

        // Neither a broken chain nor a caller-rejected identity may enter the cache.
        let (ancestry, fetched) = resolve(from, to, cached, fetched)?;
        check(&ancestry)?;
        let fetched_count = fetched.len();

        // Another load may have filled an entry while the requests were in flight.
        let mut cache = self.cache.write();
        for header in fetched {
            if let Some(existing) = cache.peek(&header.number) {
                ensure!(
                    existing.hash == header.hash,
                    "conflicting L1 header at cached block {}: cached={}, fetched={}",
                    header.number,
                    existing.hash,
                    header.hash
                );
                continue;
            }
            let number = header.number;
            ensure!(
                cache.insert(number, header),
                "failed to cache L1 header for block {number}"
            );
        }
        drop(cache);

        info!(
            from,
            to,
            cache_hits,
            fetched = fetched_count,
            "resolved ancestry headers"
        );
        Ok(ancestry)
    }

    async fn fetch(&self, number: u64) -> Result<CachedHeader> {
        let header = self
            .provider
            .get_header_by_number(number.into())
            .await?
            .ok_or_eyre(format!("L1 header not found for block {number}"))?
            .inner
            .inner;
        ensure!(
            header.number() == number,
            "L1 returned header {} for requested block {number}",
            header.number()
        );
        Ok(CachedHeader::new(&header))
    }
}

/// One L1 header retained for ancestry construction.
#[derive(Debug, Clone)]
struct CachedHeader {
    number: u64,
    parent_hash: B256,
    hash: B256,
    encoded: Bytes,
}

impl CachedHeader {
    fn new(header: &TempoHeader) -> Self {
        let mut encoded = Vec::with_capacity(600);
        header.encode(&mut encoded);
        Self {
            number: header.number(),
            parent_hash: header.parent_hash(),
            hash: keccak256(&encoded),
            encoded: encoded.into(),
        }
    }
}

/// Merge cached and fetched headers into one complete, parent-linked range.
///
/// Rejects missing, duplicate and out-of-range headers and broken parent links.
/// Also returns the fetched headers, in height order, for admission to the cache.
fn resolve(
    from: u64,
    to: u64,
    cached: Vec<CachedHeader>,
    fetched: Vec<CachedHeader>,
) -> Result<(Ancestry, Vec<CachedHeader>)> {
    ensure!(
        from <= to,
        "invalid ancestry range: base block {from} is after anchor block {to}"
    );
    let range_len = usize::try_from(to - from + 1)?;
    let fetched_count = fetched.len();
    let mut slots = vec![None; range_len];

    let mut insert = |header: CachedHeader, was_fetched| -> Result<()> {
        let number = header.number;
        ensure!(
            (from..=to).contains(&number),
            "received out-of-range L1 header for block {number}; expected {from}..={to}"
        );
        let slot = &mut slots[(number - from) as usize];
        ensure!(
            slot.replace((header, was_fetched)).is_none(),
            "received duplicate L1 header for block {number}"
        );
        Ok(())
    };
    for header in cached {
        insert(header, false)?;
    }
    for header in fetched {
        insert(header, true)?;
    }

    let mut slots = slots.into_iter();
    let (base, base_was_fetched) = slots
        .next()
        .flatten()
        .ok_or_eyre(format!("L1 header not found for base block {from}"))?;
    let base_id = BlockNumHash::new(from, base.hash);
    let mut parent_hash = base.hash;
    let mut headers = Vec::with_capacity(range_len - 1);
    let mut fetched = Vec::with_capacity(fetched_count);
    if base_was_fetched {
        fetched.push(base);
    }

    for (number, slot) in (from + 1..=to).zip(slots) {
        let (header, was_fetched) =
            slot.ok_or_eyre(format!("L1 header not found for block {number}"))?;
        ensure!(
            header.parent_hash == parent_hash,
            "parent-hash chain broken at block {number}: expected parent_hash={parent_hash}, got={}",
            header.parent_hash
        );
        parent_hash = header.hash;
        headers.push(header.encoded.clone());
        if was_fetched {
            fetched.push(header);
        }
    }

    let ancestry = Ancestry {
        base: base_id,
        anchor: BlockNumHash::new(to, parent_hash),
        headers,
    };
    Ok((ancestry, fetched))
}

#[cfg(test)]
pub(crate) mod test_utils {
    use super::*;
    use alloy_consensus::Header as ConsensusHeader;
    use alloy_rpc_types_eth::Header as RpcHeader;
    use tempo_alloy::rpc::TempoHeaderResponse;

    /// RPC header response for `number` with the given parent, plus its hash.
    pub(crate) fn mock_l1_header(number: u64, parent_hash: B256) -> (TempoHeaderResponse, B256) {
        let header = TempoHeader {
            inner: ConsensusHeader {
                number,
                parent_hash,
                ..Default::default()
            },
            ..Default::default()
        };
        let hash = keccak256(alloy_rlp::encode(&header));
        (
            TempoHeaderResponse {
                inner: RpcHeader {
                    hash,
                    inner: header,
                    total_difficulty: None,
                    size: None,
                },
                timestamp_millis: 0,
            },
            hash,
        )
    }

    /// Parent-linked mock responses for `from..=to`.
    pub(crate) fn mock_l1_chain(from: u64, to: u64) -> Vec<(TempoHeaderResponse, B256)> {
        let mut parent_hash = B256::ZERO;
        (from..=to)
            .map(|number| {
                let header = mock_l1_header(number, parent_hash);
                parent_hash = header.1;
                header
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{test_utils::mock_l1_chain, *};
    use alloy_provider::ProviderBuilder;
    use alloy_transport::mock::Asserter;
    use proptest::prelude::*;

    fn mocked_loader() -> (AncestryLoader, Asserter) {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        (AncestryLoader::with_capacity(provider, 4), asserter)
    }

    fn encoded(header: &tempo_alloy::rpc::TempoHeaderResponse) -> Bytes {
        Bytes::from(alloy_rlp::encode(&header.inner.inner))
    }

    fn synthetic_ancestry(from: u64, payloads: &[Vec<u8>]) -> Vec<CachedHeader> {
        let mut parent_hash = B256::ZERO;
        payloads
            .iter()
            .enumerate()
            .map(|(index, payload)| {
                let number = from + u64::try_from(index).unwrap();
                let mut encoded = Vec::with_capacity(size_of::<u64>() + payload.len());
                encoded.extend_from_slice(&number.to_be_bytes());
                encoded.extend_from_slice(payload);
                let encoded = Bytes::from(encoded);
                let hash = keccak256(&encoded);
                let header = CachedHeader {
                    number,
                    parent_hash,
                    hash,
                    encoded,
                };
                parent_hash = hash;
                header
            })
            .collect()
    }

    fn ancestry_case() -> impl Strategy<Value = (u64, Vec<Vec<u8>>, Vec<u64>)> {
        (0_u64..10_000, 2_usize..33).prop_flat_map(|(from, len)| {
            (
                Just(from),
                proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), len),
                proptest::collection::vec(any::<u64>(), len),
            )
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn ancestry_resolution_is_independent_of_cache_partition(
            (from, payloads, order_keys) in ancestry_case(),
            cache_mask in any::<u128>(),
        ) {
            let chain = synthetic_ancestry(from, &payloads);
            let to = chain.last().unwrap().number;
            let (cold, _) = resolve(from, to, Vec::new(), chain.clone()).unwrap();
            let (cached, fetched): (Vec<_>, Vec<_>) = chain
                .into_iter()
                .enumerate()
                .partition(|(index, _)| cache_mask & (1_u128 << index) != 0);
            let cached = cached.into_iter().map(|(_, header)| header).collect();
            let mut fetched = fetched
                .into_iter()
                .map(|(index, header)| (order_keys[index], header))
                .collect::<Vec<_>>();
            fetched.sort_by_key(|(order_key, _)| *order_key);
            let fetched = fetched.into_iter().map(|(_, header)| header).collect::<Vec<_>>();
            let mut expected_fetched = fetched
                .iter()
                .map(|header| (header.number, header.hash))
                .collect::<Vec<_>>();
            expected_fetched.sort_by_key(|(number, _)| *number);

            let (partitioned, actual_fetched) = resolve(from, to, cached, fetched).unwrap();
            let actual_fetched = actual_fetched
                .iter()
                .map(|header| (header.number, header.hash))
                .collect::<Vec<_>>();
            prop_assert_eq!(partitioned, cold);
            prop_assert_eq!(actual_fetched, expected_fetched);
        }

        #[test]
        fn ancestry_resolution_rejects_parent_hash_corruption(
            (from, payloads, _) in ancestry_case(),
            corrupt_index in any::<usize>(),
        ) {
            let mut chain = synthetic_ancestry(from, &payloads);
            let to = chain.last().unwrap().number;
            let corrupt_index = 1 + corrupt_index % (chain.len() - 1);
            chain[corrupt_index].parent_hash.0[0] ^= 1;

            prop_assert!(resolve(from, to, Vec::new(), chain).is_err());
        }

        #[test]
        fn ancestry_resolution_rejects_malformed_header_sets(
            (from, payloads, _) in ancestry_case(),
            malformed_index in any::<usize>(),
        ) {
            let chain = synthetic_ancestry(from, &payloads);
            let to = chain.last().unwrap().number;
            let malformed_index = malformed_index % chain.len();

            let mut missing = chain.clone();
            missing.remove(malformed_index);
            prop_assert!(
                resolve(from, to, Vec::new(), missing).is_err(),
                "missing header was accepted"
            );

            let mut duplicate = chain.clone();
            duplicate.push(chain[malformed_index].clone());
            prop_assert!(
                resolve(from, to, Vec::new(), duplicate).is_err(),
                "duplicate header was accepted"
            );

            let mut out_of_range = chain.clone();
            out_of_range.push(CachedHeader { number: to + 1, ..chain[malformed_index].clone() });
            prop_assert!(
                resolve(from, to, Vec::new(), out_of_range).is_err(),
                "out-of-range header was accepted"
            );
        }

        #[test]
        fn ancestry_resolution_returns_exact_range_without_base(
            (from, payloads, _) in ancestry_case(),
        ) {
            let chain = synthetic_ancestry(from, &payloads);
            let to = chain.last().unwrap().number;
            let expected = chain[1..]
                .iter()
                .map(|header| header.encoded.clone())
                .collect::<Vec<_>>();

            let (resolved, _) = resolve(from, to, Vec::new(), chain.clone()).unwrap();
            prop_assert_eq!(resolved.headers, expected);
            prop_assert_eq!(resolved.base, BlockNumHash::new(from, chain[0].hash));
            prop_assert_eq!(resolved.anchor, BlockNumHash::new(to, chain.last().unwrap().hash));
        }
    }

    #[test]
    fn resolution_rejects_inverted_range() {
        let chain = synthetic_ancestry(10, &[Vec::new()]);
        assert!(resolve(11, 10, Vec::new(), chain).is_err());
    }

    #[tokio::test]
    async fn empty_range_returns_base_as_anchor() {
        let (loader, asserter) = mocked_loader();
        let chain = mock_l1_chain(10, 10);
        asserter.push_success(&chain[0].0);

        let ancestry = loader.load(10, 10).await.unwrap();
        assert_eq!(ancestry.base, BlockNumHash::new(10, chain[0].1));
        assert_eq!(ancestry.anchor, ancestry.base);
        assert!(ancestry.headers.is_empty());
    }

    #[tokio::test]
    async fn misnumbered_response_is_rejected_and_not_cached() {
        let (loader, asserter) = mocked_loader();
        let chain = mock_l1_chain(10, 11);
        // The request for block 10 is answered with block 11.
        asserter.push_success(&chain[1].0);
        asserter.push_success(&chain[1].0);

        let error = loader.load(10, 11).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("L1 returned header 11 for requested block 10")
        );
        assert!(loader.is_empty());
    }

    #[tokio::test]
    async fn broken_chain_is_not_cached() {
        let (loader, asserter) = mocked_loader();
        let chain = mock_l1_chain(10, 11);
        let (unlinked, _) = test_utils::mock_l1_header(12, B256::repeat_byte(0xff));
        for (header, _) in &chain {
            asserter.push_success(header);
        }
        asserter.push_success(&unlinked);

        assert!(loader.load(10, 12).await.is_err());
        assert!(loader.is_empty());
    }

    #[tokio::test]
    async fn cache_fetches_only_new_suffix() {
        let (loader, asserter) = mocked_loader();
        let chain = mock_l1_chain(10, 15);

        // The initial range fetches its base plus all ancestry headers.
        for (header, _) in &chain[..5] {
            asserter.push_success(header);
        }
        let first = loader.load(10, 14).await.unwrap();
        assert_eq!(
            first.headers,
            chain[1..5]
                .iter()
                .map(|(header, _)| encoded(header))
                .collect::<Vec<_>>()
        );
        assert_eq!(loader.cache.read().len(), 4);

        // The overlapping range reuses blocks 11..=14 and fetches only block 15.
        // If the implementation repeats any cached RPC call, the mock has no
        // additional response queued and the test fails.
        asserter.push_success(&chain[5].0);
        let second = loader.load(11, 15).await.unwrap();
        assert_eq!(
            second.headers,
            chain[2..6]
                .iter()
                .map(|(header, _)| encoded(header))
                .collect::<Vec<_>>()
        );

        let cache = loader.cache.read();
        assert!(cache.peek(&11).is_none());
        for number in 12..=15 {
            assert!(cache.peek(&number).is_some());
        }
    }

    #[tokio::test]
    async fn cache_hits_do_not_rewrite_entries() {
        let (loader, asserter) = mocked_loader();
        for (header, _) in mock_l1_chain(10, 13) {
            asserter.push_success(&header);
        }

        let oldest =
            |loader: &AncestryLoader| loader.cache.read().peek_oldest().map(|(number, _)| *number);
        loader.load(10, 13).await.unwrap();
        assert_eq!(oldest(&loader), Some(10));

        // Resolving a fully cached range must not promote or replace every hit.
        loader.load(10, 12).await.unwrap();
        assert_eq!(oldest(&loader), Some(10));
        assert!(asserter.read_q().is_empty());
    }
}
