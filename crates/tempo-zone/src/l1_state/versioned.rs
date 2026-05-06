//! Block-versioned value primitive for L1 state tracking.
//!
//! [`HeightVersioned<V>`] tracks a value that changes across L1 block heights with a two-tier
//! model:
//!
//! - **Baseline** — a single `Option<V>` representing the merged value at the zone's last
//!   processed block. All lookups at or below the baseline height return this value.
//!
//! - **Pending** — a `BTreeMap<u64, V>` of per-block changes ahead of the zone. Lookups above
//!   the baseline height check pending entries with "latest at or before" semantics, falling
//!   back to the baseline.
//!
//! When the zone advances, [`advance`](HeightVersioned::advance) folds all pending entries up
//! to the new height into the baseline, keeping the pending map small.

use std::collections::BTreeMap;

/// A value that tracks changes across L1 block heights with automatic compaction.
///
/// The baseline represents the finalized value at or below `baseline_height`. Pending entries
/// track future changes that the zone hasn't processed yet. As the zone advances,
/// [`advance`](Self::advance) folds pending entries into the baseline.
#[derive(Debug, Clone)]
pub struct HeightVersioned<V> {
    /// The merged value at `baseline_height`. `None` if no value has ever been set.
    baseline: Option<V>,
    /// The block height up to which the baseline is valid.
    baseline_height: u64,
    /// Per-block changes above `baseline_height`.
    pending: BTreeMap<u64, V>,
}

impl<V> Default for HeightVersioned<V> {
    fn default() -> Self {
        Self {
            baseline: None,
            baseline_height: 0,
            pending: BTreeMap::new(),
        }
    }
}

impl<V: Copy> HeightVersioned<V> {
    /// Returns the value at the given block height.
    ///
    /// - At or below `baseline_height`: returns the baseline.
    /// - Above `baseline_height`: returns the latest pending entry at or before `block_number`,
    ///   falling back to the baseline.
    pub fn get(&self, block_number: u64) -> Option<V> {
        if block_number <= self.baseline_height {
            return self.baseline;
        }

        // Check pending for latest entry at or before block_number
        self.pending
            .range(..=block_number)
            .next_back()
            .map(|(_, v)| *v)
            .or(self.baseline)
    }

    /// Records a value at the given block height.
    ///
    /// Values at or below the baseline height are ignored. The baseline represents finalized
    /// engine-consumed state and is only updated by [`advance`](Self::advance), which prevents
    /// delayed RPC fallback results from overwriting newer event-derived state.
    pub fn set(&mut self, block_number: u64, value: V) {
        if block_number <= self.baseline_height {
            return;
        }

        self.pending.insert(block_number, value);
    }

    /// Advance the baseline to `new_height`, folding all pending entries up to that height.
    ///
    /// After this call, `baseline_height == new_height` and any pending entries ≤ `new_height`
    /// have been merged into the baseline. Does nothing if `new_height` is at or below the
    /// current baseline.
    pub fn advance(&mut self, new_height: u64) {
        if new_height <= self.baseline_height {
            return;
        }

        // Find the latest pending entry at or before new_height — that becomes the new baseline
        let folded = self
            .pending
            .range(..=new_height)
            .next_back()
            .map(|(_, v)| *v);

        if let Some(value) = folded {
            self.baseline = Some(value);
        }

        // Remove all pending entries up to and including new_height
        let to_remove: Vec<u64> = self.pending.range(..=new_height).map(|(k, _)| *k).collect();
        for k in to_remove {
            self.pending.remove(&k);
        }

        self.baseline_height = new_height;
    }

    /// Returns `true` if no value has ever been recorded.
    pub fn is_empty(&self) -> bool {
        self.baseline.is_none() && self.pending.is_empty()
    }

    /// Removes all recorded values and resets the baseline height.
    pub fn clear(&mut self) {
        self.baseline = None;
        self.baseline_height = 0;
        self.pending.clear();
    }

    /// Collapse all history before `min_block` into the baseline.
    ///
    /// Equivalent to [`advance`](Self::advance). Provided for API compatibility with callers
    /// that don't track zone progress explicitly.
    pub fn flatten(&mut self, min_block: u64) {
        self.advance(min_block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_none() {
        let v = HeightVersioned::<u64>::default();
        assert_eq!(v.get(100), None);
    }

    #[test]
    fn get_at_exact_block() {
        let mut v = HeightVersioned::default();
        v.set(10, 42u64);
        assert_eq!(v.get(10), Some(42));
    }

    #[test]
    fn get_returns_latest_before() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);
        v.set(20, 2);
        assert_eq!(v.get(15), Some(1));
        assert_eq!(v.get(20), Some(2));
        assert_eq!(v.get(25), Some(2));
    }

    #[test]
    fn get_before_earliest_returns_none() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);
        assert_eq!(v.get(9), None);
    }

    #[test]
    fn advance_folds_into_baseline() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);
        v.set(20, 2);
        v.set(30, 3);

        v.advance(20);

        // Baseline is now 2 at height 20
        assert_eq!(v.get(10), Some(2)); // at-or-below baseline → baseline value
        assert_eq!(v.get(20), Some(2));
        assert_eq!(v.get(25), Some(2)); // between baseline and next pending
        assert_eq!(v.get(30), Some(3)); // pending still works
    }

    #[test]
    fn advance_preserves_baseline_when_no_pending() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);

        v.advance(15);

        // No pending at 15, but baseline was set at 10
        assert_eq!(v.get(15), Some(1));
        assert_eq!(v.get(20), Some(1));
    }

    #[test]
    fn advance_below_current_is_noop() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);
        v.advance(20);
        v.advance(15); // should not regress

        assert_eq!(v.get(20), Some(1));
    }

    #[test]
    fn set_at_or_below_baseline_is_ignored() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);
        v.advance(20);

        // Delayed writes from finalized heights must not rewrite the baseline.
        v.set(15, 99);
        v.set(20, 100);
        assert_eq!(v.get(20), Some(1));

        v.set(21, 2);
        assert_eq!(v.get(21), Some(2));
    }

    #[test]
    fn flatten_is_advance() {
        let mut v = HeightVersioned::default();
        v.set(5, 1u64);
        v.set(10, 2);
        v.set(20, 3);

        v.flatten(15);

        assert_eq!(v.get(5), Some(2));
        assert_eq!(v.get(10), Some(2));
        assert_eq!(v.get(15), Some(2));
        assert_eq!(v.get(20), Some(3));
    }

    #[test]
    fn clear_resets_everything() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);
        v.advance(10);
        v.set(20, 2);

        v.clear();

        assert!(v.is_empty());
        assert_eq!(v.get(10), None);
        assert_eq!(v.get(20), None);
    }

    #[test]
    fn pending_falls_back_to_baseline() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);
        v.advance(10);

        // No pending entries, but baseline exists
        assert_eq!(v.get(15), Some(1));
        assert_eq!(v.get(100), Some(1));
    }

    #[test]
    fn multiple_advances() {
        let mut v = HeightVersioned::default();
        v.set(10, 1u64);
        v.set(20, 2);
        v.set(30, 3);
        v.set(40, 4);

        v.advance(15);
        assert_eq!(v.get(15), Some(1));

        v.advance(25);
        assert_eq!(v.get(25), Some(2));

        v.advance(35);
        assert_eq!(v.get(35), Some(3));
        assert_eq!(v.get(40), Some(4));
    }
}
