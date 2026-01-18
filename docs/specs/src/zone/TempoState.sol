// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title TempoState
/// @notice Zone-side predeploy for Tempo (L1) state verification
/// @dev Deployed at 0x1c00000000000000000000000000000000000000
///      Stores the latest finalized Tempo block info. Sequencer submits Tempo headers
///      which are validated for chain continuity and decoded to update state.
contract TempoState {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The sequencer address (only caller for finalizeTempo)
    address public immutable sequencer;

    /// @notice Current finalized Tempo block hash
    bytes32 public tempoBlockHash;

    /// @notice Current finalized Tempo block number
    uint64 public tempoBlockNumber;

    /// @notice Current finalized Tempo block timestamp (seconds)
    uint64 public tempoTimestamp;

    /// @notice Current finalized Tempo state root (for storage proofs)
    bytes32 public tempoStateRoot;

    /// @notice Current finalized Tempo receipts root (for event/log proofs)
    bytes32 public tempoReceiptsRoot;

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

    event TempoBlockFinalized(
        bytes32 indexed blockHash,
        uint64 indexed blockNumber,
        uint64 timestamp,
        bytes32 stateRoot,
        bytes32 receiptsRoot
    );

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error OnlySequencer();
    error InvalidParentHash();
    error InvalidBlockNumber();
    error InvalidRlpData();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(
        address _sequencer,
        bytes32 _genesisTempoBlockHash,
        uint64 _genesisTempoBlockNumber,
        uint64 _genesisTempoTimestamp,
        bytes32 _genesisTempoStateRoot,
        bytes32 _genesisTempoReceiptsRoot
    ) {
        sequencer = _sequencer;
        tempoBlockHash = _genesisTempoBlockHash;
        tempoBlockNumber = _genesisTempoBlockNumber;
        tempoTimestamp = _genesisTempoTimestamp;
        tempoStateRoot = _genesisTempoStateRoot;
        tempoReceiptsRoot = _genesisTempoReceiptsRoot;
    }

    /*//////////////////////////////////////////////////////////////
                            TEMPO FINALIZATION
    //////////////////////////////////////////////////////////////*/

    /// @notice Finalize a Tempo block header
    /// @dev Validates chain continuity (parent hash must match stored hash, number must be +1)
    ///      The header is RLP-encoded as: rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])
    ///      where inner is a standard Ethereum header.
    /// @param header RLP-encoded Tempo header
    function finalizeTempo(bytes calldata header) external {
        if (msg.sender != sequencer) revert OnlySequencer();

        // Decode the Tempo header
        (
            bytes32 parentHash,
            bytes32 stateRoot,
            bytes32 receiptsRoot,
            uint64 number,
            uint64 timestamp
        ) = _decodeTempoHeader(header);

        // Validate chain continuity
        if (parentHash != tempoBlockHash) revert InvalidParentHash();
        if (number != tempoBlockNumber + 1) revert InvalidBlockNumber();

        // Compute new block hash (keccak of full RLP bytes)
        bytes32 newBlockHash = keccak256(header);

        // Update state
        tempoBlockHash = newBlockHash;
        tempoBlockNumber = number;
        tempoTimestamp = timestamp;
        tempoStateRoot = stateRoot;
        tempoReceiptsRoot = receiptsRoot;

        emit TempoBlockFinalized(newBlockHash, number, timestamp, stateRoot, receiptsRoot);
    }

    /*//////////////////////////////////////////////////////////////
                          STORAGE READING STUBS
    //////////////////////////////////////////////////////////////*/

    /// @notice Read a storage slot from a Tempo contract
    /// @dev In production, this is a precompile that reads from proven Tempo state.
    ///      This stub reverts - actual implementation is in the zone node.
    /// @param account The Tempo contract address
    /// @param slot The storage slot to read
    /// @return value The storage value (stub always reverts)
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32) {
        // Silence unused variable warnings
        account; slot;
        revert("TempoState: readTempoStorageSlot is a precompile stub");
    }

    /// @notice Read multiple storage slots from a Tempo contract
    /// @dev In production, this is a precompile that reads from proven Tempo state.
    ///      This stub reverts - actual implementation is in the zone node.
    /// @param account The Tempo contract address
    /// @param slots The storage slots to read
    /// @return values The storage values (stub always reverts)
    function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory) {
        // Silence unused variable warnings
        account; slots;
        revert("TempoState: readTempoStorageSlots is a precompile stub");
    }

    /*//////////////////////////////////////////////////////////////
                          RLP DECODING (INTERNAL)
    //////////////////////////////////////////////////////////////*/

    /// @notice Decode a Tempo header to extract relevant fields
    /// @dev Tempo header format: rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])
    ///      Inner Ethereum header fields (0-indexed):
    ///        0: parentHash, 1: ommersHash, 2: beneficiary, 3: stateRoot,
    ///        4: transactionsRoot, 5: receiptsRoot, 6: logsBloom, 7: difficulty,
    ///        8: number, 9: gasLimit, 10: gasUsed, 11: timestamp, ...
    function _decodeTempoHeader(bytes calldata header) internal pure returns (
        bytes32 parentHash,
        bytes32 stateRoot,
        bytes32 receiptsRoot,
        uint64 number,
        uint64 timestamp
    ) {
        uint256 ptr = 0;

        // Decode outer list header
        (uint256 outerListLen, uint256 outerListOffset) = _decodeListHeader(header, ptr);
        if (outerListOffset == 0) revert InvalidRlpData();
        ptr = outerListOffset;
        uint256 outerEnd = ptr + outerListLen;

        // Skip first 3 fields: general_gas_limit, shared_gas_limit, timestamp_millis_part
        for (uint256 i = 0; i < 3; i++) {
            (, uint256 nextPtr) = _skipRlpItem(header, ptr);
            ptr = nextPtr;
        }

        // Fourth field is the inner Ethereum header (a list)
        (uint256 innerListLen, uint256 innerListOffset) = _decodeListHeader(header, ptr);
        if (innerListOffset == 0) revert InvalidRlpData();
        ptr = innerListOffset;
        uint256 innerEnd = ptr + innerListLen;

        // Field 0: parentHash (bytes32)
        parentHash = _decodeBytes32(header, ptr);
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 1: ommersHash - skip
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 2: beneficiary - skip
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 3: stateRoot (bytes32)
        stateRoot = _decodeBytes32(header, ptr);
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 4: transactionsRoot - skip
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 5: receiptsRoot (bytes32)
        receiptsRoot = _decodeBytes32(header, ptr);
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 6: logsBloom - skip
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 7: difficulty - skip
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 8: number (uint64)
        number = _decodeUint64(header, ptr);
        (, ptr) = _skipRlpItem(header, ptr);

        // Skip fields 9 and 10
        (, ptr) = _skipRlpItem(header, ptr);
        (, ptr) = _skipRlpItem(header, ptr);

        // Field 11: timestamp (uint64)
        timestamp = _decodeUint64(header, ptr);

        // Silence unused variable warnings
        outerEnd; innerEnd;
    }

    /// @notice Decode an RLP list header
    /// @return listLen The length of the list content
    /// @return offset The offset where list content begins
    function _decodeListHeader(bytes calldata data, uint256 ptr) internal pure returns (uint256 listLen, uint256 offset) {
        if (ptr >= data.length) return (0, 0);

        uint8 prefix = uint8(data[ptr]);

        if (prefix <= 0xbf) {
            // Not a list
            return (0, 0);
        } else if (prefix <= 0xf7) {
            // Short list: 0xc0 + length
            listLen = prefix - 0xc0;
            offset = ptr + 1;
        } else {
            // Long list: 0xf7 + length of length
            uint256 lenLen = prefix - 0xf7;
            if (ptr + 1 + lenLen > data.length) return (0, 0);

            listLen = 0;
            for (uint256 i = 0; i < lenLen; i++) {
                listLen = (listLen << 8) | uint8(data[ptr + 1 + i]);
            }
            offset = ptr + 1 + lenLen;
        }
    }

    /// @notice Skip an RLP item and return its length and next position
    function _skipRlpItem(bytes calldata data, uint256 ptr) internal pure returns (uint256 itemLen, uint256 nextPtr) {
        if (ptr >= data.length) revert InvalidRlpData();

        uint8 prefix = uint8(data[ptr]);

        if (prefix <= 0x7f) {
            // Single byte
            return (1, ptr + 1);
        } else if (prefix <= 0xb7) {
            // Short string: 0x80 + length
            uint256 strLen = prefix - 0x80;
            return (1 + strLen, ptr + 1 + strLen);
        } else if (prefix <= 0xbf) {
            // Long string: 0xb7 + length of length
            uint256 lenLen = prefix - 0xb7;
            uint256 strLen = 0;
            for (uint256 i = 0; i < lenLen; i++) {
                strLen = (strLen << 8) | uint8(data[ptr + 1 + i]);
            }
            return (1 + lenLen + strLen, ptr + 1 + lenLen + strLen);
        } else if (prefix <= 0xf7) {
            // Short list: 0xc0 + length
            uint256 listLen = prefix - 0xc0;
            return (1 + listLen, ptr + 1 + listLen);
        } else {
            // Long list: 0xf7 + length of length
            uint256 lenLen = prefix - 0xf7;
            uint256 listLen = 0;
            for (uint256 i = 0; i < lenLen; i++) {
                listLen = (listLen << 8) | uint8(data[ptr + 1 + i]);
            }
            return (1 + lenLen + listLen, ptr + 1 + lenLen + listLen);
        }
    }

    /// @notice Decode a bytes32 from RLP
    function _decodeBytes32(bytes calldata data, uint256 ptr) internal pure returns (bytes32 value) {
        if (ptr >= data.length) revert InvalidRlpData();

        uint8 prefix = uint8(data[ptr]);

        if (prefix == 0xa0) {
            // 32-byte string: 0x80 + 32 = 0xa0
            if (ptr + 33 > data.length) revert InvalidRlpData();
            assembly {
                value := calldataload(add(data.offset, add(ptr, 1)))
            }
        } else if (prefix <= 0x7f) {
            // Single byte value
            value = bytes32(uint256(prefix));
        } else if (prefix >= 0x80 && prefix <= 0xb7) {
            // Short string
            uint256 strLen = prefix - 0x80;
            if (strLen == 0) {
                value = bytes32(0);
            } else if (strLen <= 32) {
                if (ptr + 1 + strLen > data.length) revert InvalidRlpData();
                bytes32 temp;
                assembly {
                    temp := calldataload(add(data.offset, add(ptr, 1)))
                }
                // Shift to align to left
                value = temp >> (8 * (32 - strLen));
                // But we want it as a bytes32 (right-padded with zeros is fine for hashes)
                // Actually for hashes we want the raw bytes, so shift back
                value = bytes32(uint256(value) << (8 * (32 - strLen)));
            } else {
                revert InvalidRlpData();
            }
        } else {
            revert InvalidRlpData();
        }
    }

    /// @notice Decode a uint64 from RLP
    function _decodeUint64(bytes calldata data, uint256 ptr) internal pure returns (uint64 value) {
        if (ptr >= data.length) revert InvalidRlpData();

        uint8 prefix = uint8(data[ptr]);

        if (prefix <= 0x7f) {
            // Single byte value
            return uint64(prefix);
        } else if (prefix == 0x80) {
            // Empty string = 0
            return 0;
        } else if (prefix >= 0x81 && prefix <= 0x88) {
            // Short string (1-8 bytes)
            uint256 strLen = prefix - 0x80;
            if (ptr + 1 + strLen > data.length) revert InvalidRlpData();

            value = 0;
            for (uint256 i = 0; i < strLen; i++) {
                value = (value << 8) | uint64(uint8(data[ptr + 1 + i]));
            }
        } else {
            revert InvalidRlpData();
        }
    }
}
