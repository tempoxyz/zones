//! Persistence for Privacy Zone blocks and state.
//!
//! Provides file-based storage for zone blocks and state snapshots.
//! Uses bincode for efficient serialization with JSON fallback for debugging.

use crate::{
    builder::ZoneBlock,
    types::{PzConfig, PzState},
};
use alloy_primitives::B256;
use reth_tracing::tracing::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
};

/// Zone storage for persisting blocks and state.
#[derive(Debug)]
pub struct ZoneStorage {
    /// Data directory for zone storage.
    data_dir: PathBuf,
    /// Zone ID for namespacing.
    zone_id: u64,
    /// In-memory block index (block_number -> block_hash).
    block_index: BTreeMap<u64, B256>,
    /// Latest persisted block number.
    latest_block: Option<u64>,
}

/// Persisted block header (lightweight, without full bundle).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedBlockHeader {
    /// Block number.
    pub number: u64,
    /// Block timestamp.
    pub timestamp: u64,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Block hash.
    pub hash: B256,
    /// State root after execution.
    pub state_root: B256,
    /// Transactions root.
    pub transactions_root: B256,
    /// Transaction count.
    pub tx_count: usize,
    /// Deposit count.
    pub deposit_count: usize,
    /// Gas used.
    pub gas_used: u64,
}

impl From<&ZoneBlock> for PersistedBlockHeader {
    fn from(block: &ZoneBlock) -> Self {
        Self {
            number: block.number,
            timestamp: block.timestamp,
            parent_hash: block.parent_hash,
            hash: block.hash,
            state_root: block.state_root,
            transactions_root: block.transactions_root,
            tx_count: block.tx_count,
            deposit_count: block.deposit_count,
            gas_used: block.gas_used,
        }
    }
}

/// Snapshot of zone state for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Zone configuration.
    pub config: PzConfig,
    /// Zone state (cursors, hashes, block height).
    pub state: PzState,
    /// Latest block number in this snapshot.
    pub block_number: u64,
    /// Block hash at this snapshot.
    pub block_hash: B256,
}

impl ZoneStorage {
    /// Create new zone storage.
    ///
    /// If `data_dir` is None, storage operations will be no-ops (in-memory only mode).
    pub fn new(zone_id: u64, data_dir: Option<PathBuf>) -> eyre::Result<Self> {
        let data_dir = match data_dir {
            Some(dir) => dir,
            None => {
                info!(zone_id, "Running in memory-only mode (no persistence)");
                return Ok(Self {
                    data_dir: PathBuf::new(),
                    zone_id,
                    block_index: BTreeMap::new(),
                    latest_block: None,
                });
            }
        };

        // Create directory structure
        let zone_dir = data_dir.join(format!("zone_{zone_id}"));
        fs::create_dir_all(zone_dir.join("blocks"))?;
        fs::create_dir_all(zone_dir.join("state"))?;

        // Load block index if exists
        let mut storage = Self {
            data_dir: zone_dir,
            zone_id,
            block_index: BTreeMap::new(),
            latest_block: None,
        };

        storage.load_block_index()?;

        info!(
            zone_id,
            data_dir = %storage.data_dir.display(),
            latest_block = ?storage.latest_block,
            "Initialized zone storage"
        );

        Ok(storage)
    }

    /// Check if storage is in memory-only mode.
    pub fn is_memory_only(&self) -> bool {
        self.data_dir.as_os_str().is_empty()
    }

    /// Get the latest persisted block number.
    pub fn latest_block(&self) -> Option<u64> {
        self.latest_block
    }

    /// Get block hash by number.
    pub fn get_block_hash(&self, number: u64) -> Option<B256> {
        self.block_index.get(&number).copied()
    }

    /// Persist a zone block.
    pub fn persist_block(&mut self, block: &ZoneBlock) -> eyre::Result<()> {
        if self.is_memory_only() {
            self.block_index.insert(block.number, block.hash);
            self.latest_block = Some(block.number);
            return Ok(());
        }

        let header = PersistedBlockHeader::from(block);
        let path = self.block_path(block.number);

        let file = File::create(&path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &header)?;

        self.block_index.insert(block.number, block.hash);
        self.latest_block = Some(block.number);

        debug!(
            zone_id = self.zone_id,
            block_number = block.number,
            block_hash = %block.hash,
            path = %path.display(),
            "Persisted zone block"
        );

        Ok(())
    }

    /// Load a persisted block header.
    pub fn load_block(&self, number: u64) -> eyre::Result<Option<PersistedBlockHeader>> {
        if self.is_memory_only() {
            return Ok(None);
        }

        let path = self.block_path(number);
        if !path.exists() {
            return Ok(None);
        }

        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let header: PersistedBlockHeader = serde_json::from_reader(reader)?;

        Ok(Some(header))
    }

    /// Persist a state snapshot.
    pub fn persist_state(&self, snapshot: &StateSnapshot) -> eyre::Result<()> {
        if self.is_memory_only() {
            return Ok(());
        }

        let path = self.state_path(snapshot.block_number);

        let file = File::create(&path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, snapshot)?;

        debug!(
            zone_id = self.zone_id,
            block_number = snapshot.block_number,
            path = %path.display(),
            "Persisted state snapshot"
        );

        Ok(())
    }

    /// Load the latest state snapshot.
    pub fn load_latest_state(&self) -> eyre::Result<Option<StateSnapshot>> {
        if self.is_memory_only() {
            return Ok(None);
        }

        // Find the latest state snapshot
        let state_dir = self.data_dir.join("state");
        if !state_dir.exists() {
            return Ok(None);
        }

        let mut latest: Option<(u64, PathBuf)> = None;

        for entry in fs::read_dir(&state_dir)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                if let Some(num_str) = name.strip_prefix("state_") {
                    if let Ok(num) = num_str.parse::<u64>() {
                        match &latest {
                            None => latest = Some((num, path)),
                            Some((latest_num, _)) if num > *latest_num => {
                                latest = Some((num, path))
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        match latest {
            Some((_, path)) => {
                let file = File::open(&path)?;
                let reader = BufReader::new(file);
                let snapshot: StateSnapshot = serde_json::from_reader(reader)?;
                Ok(Some(snapshot))
            }
            None => Ok(None),
        }
    }

    /// Remove blocks after a given number (for reorgs).
    pub fn remove_blocks_after(&mut self, block_number: u64) -> eyre::Result<usize> {
        let to_remove: Vec<_> = self
            .block_index
            .range((block_number + 1)..)
            .map(|(n, _)| *n)
            .collect();

        let count = to_remove.len();

        for num in &to_remove {
            self.block_index.remove(num);

            if !self.is_memory_only() {
                let path = self.block_path(*num);
                if path.exists() {
                    if let Err(e) = fs::remove_file(&path) {
                        warn!(block_number = num, error = %e, "Failed to remove block file");
                    }
                }
            }
        }

        // Update latest block
        self.latest_block = self.block_index.keys().next_back().copied();

        if count > 0 {
            info!(
                zone_id = self.zone_id,
                removed_count = count,
                new_latest = ?self.latest_block,
                "Removed blocks after reorg"
            );
        }

        Ok(count)
    }

    // Helper methods

    fn block_path(&self, number: u64) -> PathBuf {
        self.data_dir
            .join("blocks")
            .join(format!("block_{number:010}.json"))
    }

    fn state_path(&self, block_number: u64) -> PathBuf {
        self.data_dir
            .join("state")
            .join(format!("state_{block_number:010}.json"))
    }

    fn load_block_index(&mut self) -> eyre::Result<()> {
        let blocks_dir = self.data_dir.join("blocks");
        if !blocks_dir.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&blocks_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(Some(header)) = self.load_block_from_path(&path) {
                    self.block_index.insert(header.number, header.hash);
                    match self.latest_block {
                        None => self.latest_block = Some(header.number),
                        Some(latest) if header.number > latest => {
                            self.latest_block = Some(header.number)
                        }
                        _ => {}
                    }
                }
            }
        }

        debug!(
            zone_id = self.zone_id,
            block_count = self.block_index.len(),
            latest_block = ?self.latest_block,
            "Loaded block index"
        );

        Ok(())
    }

    fn load_block_from_path(&self, path: &Path) -> eyre::Result<Option<PersistedBlockHeader>> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let header: PersistedBlockHeader = serde_json::from_reader(reader)?;
        Ok(Some(header))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use reth_revm::db::BundleState;
    use tempfile::tempdir;

    fn test_config() -> PzConfig {
        PzConfig {
            zone_id: 1,
            portal_address: Address::ZERO,
            gas_token: Address::ZERO,
            sequencer: Address::ZERO,
            genesis_state_root: B256::ZERO,
            data_dir: None,
        }
    }

    fn test_block(number: u64) -> ZoneBlock {
        ZoneBlock {
            number,
            timestamp: 1000 + number * 250,
            parent_hash: B256::repeat_byte(number.saturating_sub(1) as u8),
            hash: B256::repeat_byte(number as u8),
            state_root: B256::repeat_byte(0x42),
            transactions_root: B256::repeat_byte(0x43),
            tx_count: 1,
            deposit_count: 1,
            gas_used: 21000,
            bundle: BundleState::default(),
        }
    }

    #[test]
    fn test_memory_only_mode() {
        let mut storage = ZoneStorage::new(1, None).unwrap();
        assert!(storage.is_memory_only());

        let block = test_block(1);
        storage.persist_block(&block).unwrap();

        assert_eq!(storage.latest_block(), Some(1));
        assert_eq!(storage.get_block_hash(1), Some(block.hash));

        // Load returns None in memory-only mode
        assert!(storage.load_block(1).unwrap().is_none());
    }

    #[test]
    fn test_persist_and_load_block() {
        let dir = tempdir().unwrap();
        let mut storage = ZoneStorage::new(1, Some(dir.path().to_path_buf())).unwrap();

        let block = test_block(1);
        storage.persist_block(&block).unwrap();

        assert_eq!(storage.latest_block(), Some(1));
        assert_eq!(storage.get_block_hash(1), Some(block.hash));

        let loaded = storage.load_block(1).unwrap().unwrap();
        assert_eq!(loaded.number, 1);
        assert_eq!(loaded.hash, block.hash);
        assert_eq!(loaded.tx_count, 1);
    }

    #[test]
    fn test_persist_state_snapshot() {
        let dir = tempdir().unwrap();
        let storage = ZoneStorage::new(1, Some(dir.path().to_path_buf())).unwrap();

        let snapshot = StateSnapshot {
            config: test_config(),
            state: PzState::default(),
            block_number: 10,
            block_hash: B256::repeat_byte(0x10),
        };

        storage.persist_state(&snapshot).unwrap();

        let loaded = storage.load_latest_state().unwrap().unwrap();
        assert_eq!(loaded.block_number, 10);
    }

    #[test]
    fn test_remove_blocks_after() {
        let dir = tempdir().unwrap();
        let mut storage = ZoneStorage::new(1, Some(dir.path().to_path_buf())).unwrap();

        // Persist 5 blocks
        for i in 1..=5 {
            storage.persist_block(&test_block(i)).unwrap();
        }

        assert_eq!(storage.latest_block(), Some(5));

        // Remove blocks after 3
        let removed = storage.remove_blocks_after(3).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(storage.latest_block(), Some(3));

        // Verify blocks 4 and 5 are gone
        assert!(storage.load_block(4).unwrap().is_none());
        assert!(storage.load_block(5).unwrap().is_none());

        // Block 3 should still exist
        assert!(storage.load_block(3).unwrap().is_some());
    }

    #[test]
    fn test_reload_storage() {
        let dir = tempdir().unwrap();

        // First instance - write blocks
        {
            let mut storage = ZoneStorage::new(1, Some(dir.path().to_path_buf())).unwrap();
            storage.persist_block(&test_block(1)).unwrap();
            storage.persist_block(&test_block(2)).unwrap();
            storage.persist_block(&test_block(3)).unwrap();
        }

        // Second instance - should reload index
        {
            let storage = ZoneStorage::new(1, Some(dir.path().to_path_buf())).unwrap();
            assert_eq!(storage.latest_block(), Some(3));
            assert_eq!(storage.block_index.len(), 3);
        }
    }
}
