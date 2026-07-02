//! `TempoState` — Zone L2 predeploy (0x1c00...0000).

crate::sol! {
    #[derive(Debug)]
    contract TempoState {
        event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);

        error InvalidParentHash();
        error InvalidBlockNumber();
        error InvalidRlpData();
        error OnlyZoneInbox();

        function tempoBlockHash() external view returns (bytes32);
        function tempoBlockNumber() external view returns (uint64);

        function finalizeTempo(bytes calldata header) external;
        function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);
        function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);
    }
}
