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
        function tempoStateRoot() external view returns (bytes32);
        function tempoParentHash() external view returns (bytes32);
        function tempoBeneficiary() external view returns (address);
        function tempoTransactionsRoot() external view returns (bytes32);
        function tempoReceiptsRoot() external view returns (bytes32);
        function tempoGasLimit() external view returns (uint64);
        function tempoGasUsed() external view returns (uint64);
        function tempoTimestamp() external view returns (uint64);
        function tempoTimestampMillis() external view returns (uint64);
        function tempoPrevRandao() external view returns (bytes32);
        function generalGasLimit() external view returns (uint64);
        function sharedGasLimit() external view returns (uint64);

        function finalizeTempo(bytes calldata header) external;
    }
}
