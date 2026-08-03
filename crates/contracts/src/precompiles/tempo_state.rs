//! `TempoState` — Zone L2 predeploy (0x1c00...0000).

pub use ITempoState::{ITempoStateErrors as TempoStateError, ITempoStateEvents as TempoStateEvent};

crate::sol! {
    #[derive(Debug, PartialEq, Eq)]
    contract ITempoState {
        event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);

        error InvalidParentHash();
        error InvalidBlockNumber();
        error InvalidRlpData();
        error OnlyZoneInbox();

        function tempoBlockHash() external view returns (bytes32);
        function tempoBlockNumber() external view returns (uint64);

        function finalizeTempo(bytes calldata header) external;
    }
}
