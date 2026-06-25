//! `TempoStateReader` — Zone L2 standalone precompile.
//!
//! Separate from [`TempoState`](crate::precompiles::tempo_state::TempoState); reads Tempo L1
//! storage at a caller-specified block.
//!
//! Access is restricted to the TempoState wrapper. Caller authorization is handled by that wrapper,
//! not by this reader.

crate::sol! {
    #[derive(Debug)]
    contract TempoStateReader {
        /// Returned when the precompile is invoked via `DELEGATECALL`.
        error DelegateCallNotAllowed();

        /// Returned when the caller is not TempoState.
        error Unauthorized();

        function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);
        function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);
    }
}
