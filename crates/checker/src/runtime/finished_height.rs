//! Which height the checker may report to Reth as its `ExExEvent::FinishedHeight`.
//!
//! Reth finalizes its ExEx WAL from the lowest `FinishedHeight` any extension reports, so
//! acknowledging a height the checker still needs to replay would let that history be discarded.
//! Active verification resolves a persisted, replayable frontier before emitting it. Once the
//! checker is disabled, the runtime acknowledges delivered commits directly because observe mode
//! prohibits pruning every segment needed for provider replay.

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use reth_exex::ExExContext;
use reth_node_api::FullNodeComponents;
use reth_prune_types::PruneSegment;
use reth_stages_api::StageId;
use reth_storage_api::{BlockNumReader, PruneCheckpointReader, StageCheckpointReader};

use super::{AppendOnlyViolation, RuntimeProgress};
use crate::persistence::{BlockRef, Snapshot, Store};

/// History the checker must be able to replay. Observe-mode startup rejects configuration that
/// would prune any of it; this is the preflight against pruning that already happened.
const REQUIRED_RETENTION: &[PruneSegment] = &[
    PruneSegment::AccountHistory,
    PruneSegment::StorageHistory,
    PruneSegment::Bodies,
    PruneSegment::Receipts,
    PruneSegment::ContractLogs,
];

/// One poll's decision, kept separate from emitting it so it can be tested.
#[derive(Debug)]
pub(super) struct Resolved {
    /// Reth's fully committed persistence frontier.
    pub(super) persisted: BlockNumHash,
    /// `None` when nothing new can be acknowledged yet.
    pub(super) acknowledgement: Option<BlockNumHash>,
}

/// Resolve the active checker's persisted frontier without emitting it.
pub(super) fn resolve_verifying<P>(
    provider: &P,
    progress: &RuntimeProgress,
    verified: BlockRef,
) -> eyre::Result<Resolved>
where
    P: BlockNumReader + PruneCheckpointReader + StageCheckpointReader,
{
    let resolved = resolve(provider, progress, verified)?;
    if let Some(acknowledgement) = resolved.acknowledgement
        && acknowledgement.number > verified.number
    {
        let required_from = verified
            .number
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("Zone block number overflow after verification"))?;
        ensure_replay_retained_from(provider, required_from)?;
    }
    Ok(resolved)
}

/// Emit a monotonic `FinishedHeight`, shared by active verification and the disabled drain.
pub(super) fn emit_finished_height<Node>(
    ctx: &ExExContext<Node>,
    progress: &mut RuntimeProgress,
    acknowledgement: BlockNumHash,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
{
    if !finished_height_advances(progress, acknowledgement)? {
        return Ok(());
    }
    ctx.send_finished_height(acknowledgement)?;
    progress.last_finished_tip = Some(acknowledgement);
    Ok(())
}

/// Validate the monotonic ExEx acknowledgement invariant before touching the event channel.
fn finished_height_advances(
    progress: &RuntimeProgress,
    acknowledgement: BlockNumHash,
) -> Result<bool, AppendOnlyViolation> {
    let Some(finished) = progress.last_finished_tip else {
        return Ok(true);
    };
    if acknowledgement.number < finished.number
        || (acknowledgement.number == finished.number && acknowledgement.hash != finished.hash)
    {
        return Err(AppendOnlyViolation::new(
            acknowledgement,
            format!(
                "history regressed or changed after FinishedHeight {} ({})",
                finished.number, finished.hash
            ),
        ));
    }
    Ok(acknowledgement.number > finished.number)
}

/// Persist the latest delivered tip, once confirmed to still be canonical.
pub(super) fn observe_tip<P: BlockNumReader>(
    provider: &P,
    store: &Store,
    snapshot: Snapshot,
    tip: BlockNumHash,
    persisted: BlockNumHash,
) -> eyre::Result<Snapshot> {
    let current = snapshot.metadata.observed_zone;
    if !observation_is_persisted(provider, persisted, current)? {
        return Ok(snapshot);
    }
    let observed = tip.into();
    if current == observed {
        return Ok(snapshot);
    }
    ensure_canonical_tip(provider, tip)?;
    Ok(store.observe(snapshot, observed)?)
}

/// Whether a durable observation can safely be replaced. An observation above Finish may have
/// been delivered from volatile state, so retain it until persisted history can prove its hash.
fn observation_is_persisted<P: BlockNumReader>(
    provider: &P,
    persisted: BlockNumHash,
    observed: BlockRef,
) -> eyre::Result<bool> {
    if observed.number > persisted.number {
        return Ok(false);
    }
    let canonical = canonical_hash_at(provider, persisted, observed.number)?;
    if canonical != Some(observed.hash) {
        return Err(AppendOnlyViolation::new(
            observed.into(),
            format!(
                "durable observed Zone tip {} ({}) conflicts with persisted Zone history ({canonical:?})",
                observed.number, observed.hash
            ),
        )
        .into());
    }
    Ok(true)
}

/// Whether a frontier is still behind, so the caller should keep polling for persistence.
pub(super) fn can_advance(
    persisted: BlockNumHash,
    delivered: BlockNumHash,
    verified: BlockRef,
) -> bool {
    persisted.number < delivered.number || persisted.number < verified.number
}

/// Decide which delivered, persisted, replayable height may be acknowledged.
fn resolve<P>(
    provider: &P,
    progress: &RuntimeProgress,
    verified: BlockRef,
) -> eyre::Result<Resolved>
where
    P: BlockNumReader + PruneCheckpointReader + StageCheckpointReader,
{
    let persisted = persisted_tip(provider)?;
    let hold = || Resolved {
        persisted,
        acknowledgement: None,
    };
    if persisted.number < verified.number {
        return Ok(hold());
    }
    let canonical = canonical_hash_at(provider, persisted, verified.number)?;
    if canonical != Some(verified.hash) {
        return Err(AppendOnlyViolation::new(
            verified.into(),
            format!(
                "durable checker tip {} ({}) conflicts with persisted Zone history ({canonical:?})",
                verified.number, verified.hash
            ),
        )
        .into());
    }

    let height = bounded_height(persisted, progress.last_delivered_tip, verified)?;
    let acknowledgement = BlockNumHash::new(
        height,
        canonical_hash_at(provider, persisted, height)?.ok_or_else(|| {
            eyre::eyre!("delivered, persisted Zone block {height} has no canonical block hash")
        })?,
    );
    if acknowledgement.number == progress.last_delivered_tip.number
        && acknowledgement.hash != progress.last_delivered_tip.hash
    {
        return Err(AppendOnlyViolation::new(
            progress.last_delivered_tip,
            format!(
                "delivered Zone tip {} ({}) conflicts with persisted Zone history ({})",
                acknowledgement.number, progress.last_delivered_tip.hash, acknowledgement.hash
            ),
        )
        .into());
    }

    if progress.last_finished_tip == Some(acknowledgement) {
        return Ok(hold());
    }

    Ok(Resolved {
        persisted,
        acknowledgement: Some(acknowledgement),
    })
}

/// The latest canonical block in Reth's fully committed persistence range.
///
/// An absent checkpoint is only unambiguous on a node that has committed nothing but genesis.
/// Above that it cannot be read as "committed through genesis": doing so would freeze the
/// verification target at 0 and stall the checker silently, so it is reported instead.
fn persisted_tip<P>(provider: &P) -> eyre::Result<BlockNumHash>
where
    P: BlockNumReader + StageCheckpointReader,
{
    let number = match provider.get_stage_checkpoint(StageId::Finish)? {
        Some(checkpoint) => checkpoint.block_number,
        None => {
            let committed = provider.last_block_number()?;
            eyre::ensure!(
                committed == 0,
                "Reth committed Zone blocks through {committed} but reports no {} stage checkpoint; \
                 the checker cannot locate the persistence frontier",
                StageId::Finish
            );
            0
        }
    };
    let hash = provider
        .block_hash(number)?
        .ok_or_else(|| eyre::eyre!("persisted Zone block {number} has no canonical block hash"))?;
    Ok(BlockNumHash::new(number, hash))
}

/// The lowest of the persisted and delivered frontiers, which may never fall below the verified
/// tip: acknowledging there would release history the checker has already committed to.
fn bounded_height(
    persisted: BlockNumHash,
    delivered: BlockNumHash,
    verified: BlockRef,
) -> Result<u64, AppendOnlyViolation> {
    let height = persisted.number.min(delivered.number);
    if height < verified.number {
        return Err(AppendOnlyViolation::new(
            delivered,
            format!(
                "delivered and persisted frontier {height} is below durable verified block {} ({})",
                verified.number, verified.hash
            ),
        ));
    }
    Ok(height)
}

/// The canonical hash at `number`, reusing `persisted` when it already names that block.
fn canonical_hash_at<P: BlockNumReader>(
    provider: &P,
    persisted: BlockNumHash,
    number: u64,
) -> eyre::Result<Option<B256>> {
    if number == persisted.number {
        return Ok(Some(persisted.hash));
    }
    Ok(provider.block_hash(number)?)
}

/// Reject a recovery range that overlaps history previously pruned from Reth.
fn ensure_replay_retained_from<P>(provider: &P, required_from: u64) -> eyre::Result<()>
where
    P: PruneCheckpointReader,
{
    // One batched read: the per-segment accessor opens a database transaction per call.
    for (segment, checkpoint) in provider.get_prune_checkpoints()? {
        if !REQUIRED_RETENTION.contains(&segment) {
            continue;
        }
        if checkpoint
            .block_number
            .is_some_and(|pruned_through| pruned_through >= required_from)
        {
            eyre::bail!(
                "{segment} was pruned at or after required Zone block {required_from}; resync with checker-required history retention"
            );
        }
    }
    Ok(())
}

/// Assert the append-only Zone history still contains the latest tip delivered to the checker.
fn ensure_canonical_tip<P: BlockNumReader>(provider: &P, tip: BlockNumHash) -> eyre::Result<()> {
    let canonical = provider.block_hash(tip.number)?;
    if canonical != Some(tip.hash) {
        return Err(AppendOnlyViolation::new(
            tip,
            format!(
                "delivered block {} ({}) is no longer canonical (local hash: {canonical:?})",
                tip.number, tip.hash
            ),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reth_chainspec::ChainInfo;
    use reth_prune_types::{PruneCheckpoint, PruneMode};
    use reth_stages_api::StageCheckpoint;
    use reth_storage_api::{BlockHashReader, errors::provider::ProviderResult};

    use super::*;

    /// Canonical history plus the two committed frontiers this module reads. Every trait method
    /// `resolve` never calls is left `unreachable!`, so the stub states its own surface.
    struct StubProvider {
        hashes: BTreeMap<u64, B256>,
        finish: Option<u64>,
        pruned: Vec<(PruneSegment, u64)>,
    }

    impl StubProvider {
        /// Canonical blocks `0..=tip` with deterministic hashes, all committed.
        fn canonical(tip: u64) -> Self {
            Self {
                hashes: (0..=tip)
                    .map(|number| (number, block_hash(number)))
                    .collect(),
                finish: Some(tip),
                pruned: Vec::new(),
            }
        }

        /// Hold Reth's committed frontier below the canonical tip. `None` models a node that has
        /// not yet committed anything.
        fn with_finish(mut self, finish: Option<u64>) -> Self {
            self.finish = finish;
            self
        }

        fn with_hash(mut self, number: u64, hash: B256) -> Self {
            self.hashes.insert(number, hash);
            self
        }

        fn with_pruned(mut self, segment: PruneSegment, through: u64) -> Self {
            self.pruned.push((segment, through));
            self
        }
    }

    impl BlockHashReader for StubProvider {
        fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
            Ok(self.hashes.get(&number).copied())
        }

        fn canonical_hashes_range(&self, _start: u64, _end: u64) -> ProviderResult<Vec<B256>> {
            unreachable!("StubProvider: canonical_hashes_range")
        }
    }

    impl BlockNumReader for StubProvider {
        fn chain_info(&self) -> ProviderResult<ChainInfo> {
            unreachable!("StubProvider: chain_info")
        }

        fn best_block_number(&self) -> ProviderResult<u64> {
            unreachable!("StubProvider: best_block_number")
        }

        /// Committed headers, which `persisted_tip` probes when no `Finish` checkpoint exists.
        fn last_block_number(&self) -> ProviderResult<u64> {
            Ok(self.hashes.keys().next_back().copied().unwrap_or_default())
        }

        fn block_number(&self, _hash: B256) -> ProviderResult<Option<u64>> {
            unreachable!("StubProvider: block_number")
        }
    }

    impl StageCheckpointReader for StubProvider {
        fn get_stage_checkpoint(&self, id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
            Ok((id == StageId::Finish)
                .then_some(self.finish)
                .flatten()
                .map(StageCheckpoint::new))
        }

        fn get_stage_checkpoint_progress(&self, _id: StageId) -> ProviderResult<Option<Vec<u8>>> {
            unreachable!("StubProvider: get_stage_checkpoint_progress")
        }

        fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
            unreachable!("StubProvider: get_all_checkpoints")
        }
    }

    impl PruneCheckpointReader for StubProvider {
        fn get_prune_checkpoint(
            &self,
            _segment: PruneSegment,
        ) -> ProviderResult<Option<PruneCheckpoint>> {
            unreachable!("StubProvider: get_prune_checkpoint")
        }

        fn get_prune_checkpoints(&self) -> ProviderResult<Vec<(PruneSegment, PruneCheckpoint)>> {
            Ok(self
                .pruned
                .iter()
                .map(|(segment, through)| {
                    (
                        *segment,
                        PruneCheckpoint {
                            block_number: Some(*through),
                            tx_number: None,
                            prune_mode: PruneMode::Full,
                        },
                    )
                })
                .collect())
        }
    }

    /// The high bit keeps every canonical hash distinct from the `forked` hash below.
    fn block_hash(number: u64) -> B256 {
        B256::repeat_byte(number as u8 | 0x80)
    }

    fn tip(number: u64) -> BlockNumHash {
        BlockNumHash::new(number, block_hash(number))
    }

    fn progress_at(delivered: u64, finished: Option<u64>) -> RuntimeProgress {
        RuntimeProgress {
            last_delivered_tip: tip(delivered),
            last_finished_tip: finished.map(tip),
        }
    }

    fn verified_at(number: u64) -> BlockRef {
        BlockRef::new(number, block_hash(number))
    }

    #[test]
    fn acknowledgement_is_bounded_by_persistence_delivery_and_verification() {
        // Whichever of the two frontiers is lower wins.
        assert_eq!(bounded_height(tip(10), tip(8), verified_at(0)).unwrap(), 8);
        assert_eq!(bounded_height(tip(8), tip(10), verified_at(0)).unwrap(), 8);
        // Above the verified tip is fine; below it releases committed history.
        assert_eq!(
            bounded_height(tip(10), tip(10), verified_at(9)).unwrap(),
            10
        );
        assert!(bounded_height(tip(10), tip(8), verified_at(9)).is_err());
    }

    #[test]
    fn persistence_polling_stops_only_after_every_required_frontier() {
        assert!(
            can_advance(tip(8), tip(10), verified_at(0)),
            "delivery is ahead"
        );
        assert!(
            can_advance(tip(8), tip(8), verified_at(9)),
            "verify is ahead"
        );
        assert!(
            !can_advance(tip(10), tip(10), verified_at(9)),
            "persistence has caught up with both"
        );
    }

    #[test]
    fn holds_until_persistence_reaches_the_verified_tip() {
        let provider = StubProvider::canonical(10).with_finish(Some(4));
        let resolved = resolve(&provider, &progress_at(10, None), verified_at(5)).unwrap();

        assert_eq!(resolved.persisted.number, 4);
        assert!(resolved.acknowledgement.is_none());
    }

    #[test]
    fn acknowledges_the_delivered_persisted_minimum() {
        let provider = StubProvider::canonical(10).with_finish(Some(7));
        let resolved = resolve(&provider, &progress_at(9, Some(5)), verified_at(6)).unwrap();

        assert_eq!(resolved.acknowledgement.map(|tip| tip.number), Some(7));
    }

    #[test]
    fn re_acknowledging_the_same_height_is_a_no_op() {
        let provider = StubProvider::canonical(10).with_finish(Some(7));
        let resolved = resolve(&provider, &progress_at(9, Some(7)), verified_at(6)).unwrap();

        assert!(resolved.acknowledgement.is_none());
    }

    #[test]
    fn finished_height_must_advance_monotonically() {
        let progress = progress_at(10, Some(8));
        assert!(!finished_height_advances(&progress, tip(8)).unwrap());
        assert!(finished_height_advances(&progress, tip(9)).unwrap());
        assert!(finished_height_advances(&progress, tip(7)).is_err());
        let changed = BlockNumHash::new(8, B256::repeat_byte(0x0f));
        assert!(finished_height_advances(&progress, changed).is_err());
    }

    /// The error type matters as much as the rejection: only `AppendOnlyViolation` makes the
    /// runtime persist a durable finding.
    #[test]
    fn rejects_history_that_changed_under_the_checker() {
        let forked = B256::repeat_byte(0x0f);

        // The durable verified tip is no longer canonical.
        let provider = StubProvider::canonical(10).with_hash(6, forked);
        let durable = resolve(&provider, &progress_at(9, None), verified_at(6))
            .expect_err("a forked verified tip must be fatal");
        assert!(durable.downcast_ref::<AppendOnlyViolation>().is_some());

        // The delivered tip disagrees with committed history at the same height.
        let provider = StubProvider::canonical(10).with_finish(Some(9));
        let mut progress = progress_at(9, None);
        progress.last_delivered_tip = BlockNumHash::new(9, forked);
        let delivered = resolve(&provider, &progress, verified_at(6))
            .expect_err("a forked delivered tip must be fatal");
        assert!(delivered.downcast_ref::<AppendOnlyViolation>().is_some());
    }

    #[test]
    fn retention_loss_blocks_active_acknowledgement() {
        let provider = StubProvider::canonical(10)
            .with_finish(Some(9))
            .with_pruned(PruneSegment::Receipts, 8);

        let error = resolve_verifying(&provider, &progress_at(9, Some(5)), verified_at(6))
            .expect_err("active verification must retain its replay range");
        assert!(error.to_string().contains("was pruned at or after"));
    }

    #[test]
    fn volatile_observation_waits_for_persistence() {
        let provider = StubProvider::canonical(10).with_finish(Some(8));
        assert!(
            !observation_is_persisted(&provider, tip(8), verified_at(9)).unwrap(),
            "an observation above Finish cannot be replaced yet"
        );
    }

    #[test]
    fn persisted_observation_must_match_canonical_history() {
        let forked = BlockRef::new(9, B256::repeat_byte(0x0f));
        let provider = StubProvider::canonical(10);
        let error = observation_is_persisted(&provider, tip(10), forked)
            .expect_err("a durable observed hash cannot change");
        assert!(error.downcast_ref::<AppendOnlyViolation>().is_some());
    }

    /// An absent `Finish` checkpoint means genesis only on a node that committed nothing else.
    /// With real history it is an inconsistency the checker must report, not read as genesis.
    #[test]
    fn no_finish_checkpoint_is_genesis_only_on_an_empty_node() {
        let fresh = StubProvider::canonical(0).with_finish(None);
        assert_eq!(persisted_tip(&fresh).unwrap(), tip(0));

        let populated = StubProvider::canonical(10).with_finish(None);
        let error = persisted_tip(&populated)
            .expect_err("committed history with no checkpoint must not read as genesis");
        assert!(
            error
                .to_string()
                .contains("cannot locate the persistence frontier")
        );
    }

    #[test]
    fn retention_only_rejects_pruning_that_reaches_the_required_range() {
        let provider = StubProvider::canonical(10).with_pruned(PruneSegment::AccountHistory, 5);

        // Pruning through 5 conflicts only once the required range reaches 5.
        assert!(ensure_replay_retained_from(&provider, 6).is_ok());
        assert!(ensure_replay_retained_from(&provider, 5).is_err());
        assert!(ensure_replay_retained_from(&provider, 4).is_err());
    }
}
