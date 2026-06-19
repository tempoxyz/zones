//! `TempoStateReader` — Zone L2 standalone precompile.
//!
//! Separate from [`TempoState`](crate::precompiles::tempo_state::TempoState); reads Tempo L1
//! storage at a caller-specified block.

crate::sol! {
    #[derive(Debug)]
    contract TempoStateReader {
        error DelegateCallNotAllowed();

        function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);
        function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);
    }
}
