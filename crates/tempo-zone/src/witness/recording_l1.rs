//! Recording wrapper for L1 state reads.
//!
//! Wraps any [`L1StorageReader`] to capture all Tempo L1 storage reads performed
//! during zone block execution. The recorded reads are used to build the
//! [`BatchStateProof`] with deduplicated MPT node pool.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use alloy_primitives::{Address, B256};
use eyre::Result;

use crate::l1_state::L1StorageReader;

/// A recorded Tempo L1 storage read.
#[derive(Debug, Clone)]
pub struct RecordedL1Read {
    /// The zone block index that triggered this read.
    pub zone_block_index: u64,
    /// The Tempo block number the read was against.
    pub tempo_block_number: u64,
    /// The L1 contract address read from.
    pub account: Address,
    /// The storage slot.
    pub slot: B256,
    /// The value returned.
    pub value: B256,
}

/// Shared collection of recorded L1 reads, safe for concurrent access.
pub type SharedRecordedReads = Arc<Mutex<Vec<RecordedL1Read>>>;

/// An L1 state provider wrapper that records all storage reads.
///
/// Delegates to the inner [`L1StorageReader`] for the actual read, but captures
/// every `get_storage` call in the shared [`RecordedL1Read`] collection.
///
/// The `zone_block_index` is set by the caller before each zone block is executed
/// and used to tag reads with the correct block index. It uses `AtomicU64` so it
/// can be updated through `&self` (the provider is behind `Arc<dyn L1StorageReader>`).
#[derive(Clone)]
pub struct RecordingL1StateProvider {
    inner: Arc<dyn L1StorageReader>,
    reads: SharedRecordedReads,
    /// Current zone block index, shared across all clones.
    zone_block_index: Arc<AtomicU64>,
}

impl std::fmt::Debug for RecordingL1StateProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingL1StateProvider")
            .field("zone_block_index", &self.zone_block_index)
            .finish()
    }
}

impl RecordingL1StateProvider {
    /// Create a new recording wrapper around any [`L1StorageReader`].
    pub fn new(inner: Arc<dyn L1StorageReader>) -> Self {
        Self {
            inner,
            reads: Arc::new(Mutex::new(Vec::new())),
            zone_block_index: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Set the current zone block index for tagging subsequent reads.
    ///
    /// Uses atomic ordering so this can be called through `&self` (the provider
    /// lives behind `Arc<dyn L1StorageReader>` in the EVM factory).
    pub fn set_zone_block_index(&self, index: u64) {
        self.zone_block_index.store(index, Ordering::Release);
    }

    /// Take all recorded reads, clearing the internal buffer.
    pub fn take_reads(&self) -> Vec<RecordedL1Read> {
        std::mem::take(&mut *self.reads.lock().expect("recording lock poisoned"))
    }

    /// Get a reference to the shared recorded reads.
    pub fn recorded_reads(&self) -> SharedRecordedReads {
        self.reads.clone()
    }
}

impl L1StorageReader for RecordingL1StateProvider {
    fn get_storage(&self, address: Address, slot: B256, block_number: u64) -> Result<B256> {
        let value = self.inner.get_storage(address, slot, block_number)?;

        self.reads
            .lock()
            .expect("recording lock poisoned")
            .push(RecordedL1Read {
                zone_block_index: self.zone_block_index.load(Ordering::Acquire),
                tempo_block_number: block_number,
                account: address,
                slot,
                value,
            });

        Ok(value)
    }
}
