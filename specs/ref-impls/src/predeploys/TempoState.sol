// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITempoState, ITempoStateReader } from "../interfaces/IZone.sol";

/// @title TempoState
/// @notice Zone-side predeploy for Tempo state verification
/// @dev Deployed at 0x1c00000000000000000000000000000000000000
///      Stores the latest finalized Tempo checkpoint. Sequencer submits Tempo headers
///      which are validated for chain continuity.
contract TempoState is ITempoState {

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice Current finalized Tempo block hash (keccak256 of RLP-encoded header)
    bytes32 public tempoBlockHash;

    /// @notice Block number
    uint64 public tempoBlockNumber;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    /// @notice Initialize with genesis Tempo block
    /// @param _genesisHeader RLP-encoded genesis Tempo header
    constructor(bytes memory _genesisHeader) {
        (,, uint64 blockNumber) = _decodeHeader(_genesisHeader);
        tempoBlockHash = keccak256(_genesisHeader);
        tempoBlockNumber = blockNumber;
    }

    /*//////////////////////////////////////////////////////////////
                            TEMPO FINALIZATION
    //////////////////////////////////////////////////////////////*/

    /// @notice Finalize a Tempo block header
    /// @dev Validates chain continuity (parent hash must match stored hash, number must be +1).
    ///      The header is RLP-encoded as: rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])
    ///      where inner is a standard Ethereum header.
    ///      Only callable by ZoneInbox. Executor enforces ZoneInbox-only access.
    /// @param header RLP-encoded Tempo header
    function finalizeTempo(bytes calldata header) external {
        // Only ZoneInbox can call this function
        if (msg.sender != ZONE_INBOX) revert OnlyZoneInbox();

        bytes32 prevBlockHash = tempoBlockHash;
        uint64 prevBlockNumber = tempoBlockNumber;

        (bytes32 parentHash, bytes32 stateRoot, uint64 blockNumber) = _decodeHeader(header);

        if (parentHash != prevBlockHash) revert InvalidParentHash();
        if (blockNumber != prevBlockNumber + 1) revert InvalidBlockNumber();

        tempoBlockHash = keccak256(header);
        tempoBlockNumber = blockNumber;

        emit TempoBlockFinalized(tempoBlockHash, tempoBlockNumber, stateRoot);
    }

    /*//////////////////////////////////////////////////////////////
                    TEMPO STORAGE READING (SYSTEM ONLY)
    //////////////////////////////////////////////////////////////*/

    /// @notice Zone system contract addresses that are allowed to read Tempo state
    /// @dev These contracts need access to read from ZonePortal and TIP-403 on Tempo
    address private constant ZONE_INBOX = 0x1c00000000000000000000000000000000000001;
    address private constant ZONE_OUTBOX = 0x1c00000000000000000000000000000000000002;
    address private constant ZONE_CONFIG = 0x1c00000000000000000000000000000000000003;

    /// @notice TempoStateReader compatibility precompile address
    /// @dev Low-level precompile that reads Tempo L1 contract storage at a given block height.
    address private constant TEMPO_STATE_READER = 0x1c00000000000000000000000000000000000004;

    /// @notice Check if caller is a zone system contract
    modifier onlySystemContract() {
        if (msg.sender != ZONE_INBOX && msg.sender != ZONE_OUTBOX && msg.sender != ZONE_CONFIG) {
            revert("TempoState: only zone system contracts can read Tempo state");
        }
        _;
    }

    /// @notice Read a storage slot from a Tempo L1 contract at the latest finalized block
    /// @dev RESTRICTED: Only callable by zone system contracts (ZoneInbox, ZoneOutbox, ZoneConfig).
    ///      Forwards to the TempoStateReader precompile with the current tempoBlockNumber.
    /// @param account The Tempo L1 contract address (ZonePortal or TIP-403)
    /// @param slot The storage slot to read
    /// @return value The storage value
    function readTempoStorageSlot(
        address account,
        bytes32 slot
    )
        external
        view
        onlySystemContract
        returns (bytes32 value)
    {
        value = ITempoStateReader(TEMPO_STATE_READER).readStorageAt(account, slot, tempoBlockNumber);
    }

    /// @notice Read multiple storage slots from a Tempo L1 contract at the latest finalized block
    /// @dev RESTRICTED: Only callable by zone system contracts (ZoneInbox, ZoneOutbox, ZoneConfig).
    ///      Forwards to the TempoStateReader precompile with the current tempoBlockNumber.
    /// @param account The Tempo L1 contract address (ZonePortal or TIP-403)
    /// @param slots The storage slots to read
    /// @return values The storage values
    function readTempoStorageSlots(
        address account,
        bytes32[] calldata slots
    )
        external
        view
        onlySystemContract
        returns (bytes32[] memory values)
    {
        values = ITempoStateReader(TEMPO_STATE_READER)
            .readStorageBatchAt(account, slots, tempoBlockNumber);
    }

    /*//////////////////////////////////////////////////////////////
                          RLP DECODING (INTERNAL)
    //////////////////////////////////////////////////////////////*/

    /// @notice Decode the Tempo header fields used by the zone
    /// @dev Tempo header format: rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])
    ///      Inner Ethereum header fields (0-indexed):
    ///        0: parentHash, 1: ommersHash, 2: beneficiary, 3: stateRoot,
    ///        4: transactionsRoot, 5: receiptsRoot, 6: logsBloom, 7: difficulty,
    ///        8: number, 9: gasLimit, 10: gasUsed, 11: timestamp, 12: extraData,
    ///        13: mixHash (prevRandao), 14: nonce, remaining fields are optional and ignored
    function _decodeHeader(bytes memory header)
        internal
        pure
        returns (bytes32 parentHash, bytes32 stateRoot, uint64 blockNumber)
    {
        uint256 ptr = 0;

        // Decode outer list header
        (uint256 outerListLen, uint256 outerListOffset) = _decodeListHeaderMem(header, ptr);
        if (outerListOffset == 0) revert InvalidRlpData();
        uint256 outerListEnd = outerListOffset + outerListLen;
        if (outerListEnd != header.length) revert InvalidRlpData();
        ptr = outerListOffset;

        // Field 0: general_gas_limit
        if (ptr >= outerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, outerListEnd);

        // Field 1: shared_gas_limit
        if (ptr >= outerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, outerListEnd);

        // Field 2: timestamp_millis_part
        if (ptr >= outerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, outerListEnd);

        // Field 3: inner Ethereum header (a list)
        if (ptr >= outerListEnd) revert InvalidRlpData();
        (uint256 innerListLen, uint256 innerListOffset) = _decodeListHeaderMem(header, ptr);
        if (innerListOffset == 0) revert InvalidRlpData();
        uint256 innerListEnd = innerListOffset + innerListLen;
        if (innerListEnd > outerListEnd) revert InvalidRlpData();
        ptr = innerListOffset;

        // Inner field 0: parentHash
        if (ptr >= innerListEnd) revert InvalidRlpData();
        parentHash = _decodeBytes32Mem(header, ptr);
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 1: ommersHash - skip
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 2: beneficiary
        if (ptr >= innerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 3: stateRoot
        if (ptr >= innerListEnd) revert InvalidRlpData();
        stateRoot = _decodeBytes32Mem(header, ptr);
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 4: transactionsRoot
        if (ptr >= innerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 5: receiptsRoot
        if (ptr >= innerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 6: logsBloom - skip
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 7: difficulty - skip
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 8: number
        if (ptr >= innerListEnd) revert InvalidRlpData();
        blockNumber = _decodeUint64Mem(header, ptr);
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 9: gasLimit
        if (ptr >= innerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 10: gasUsed
        if (ptr >= innerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 11: timestamp
        if (ptr >= innerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 12: extraData - skip
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 13: mixHash (prevRandao)
        if (ptr >= innerListEnd) revert InvalidRlpData();
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Inner field 14: nonce - skip
        ptr = _skipRlpItemInList(header, ptr, innerListEnd);

        // Skip optional Ethereum header fields we don't record:
        // baseFeePerGas, withdrawalsRoot, blobGasUsed, excessBlobGas,
        // parentBeaconBlockRoot, requestsHash, blockAccessListHash, slotNumber.
        for (uint256 i = 0; i < 8 && ptr < innerListEnd; i++) {
            ptr = _skipRlpItemInList(header, ptr, innerListEnd);
        }
        if (ptr != innerListEnd) revert InvalidRlpData();

        ptr = innerListEnd;

        // TempoHeader has one optional trailing outer field: consensus_context.
        if (ptr < outerListEnd) {
            ptr = _skipRlpItemInList(header, ptr, outerListEnd);
        }
        if (ptr != outerListEnd) revert InvalidRlpData();
    }

    /*//////////////////////////////////////////////////////////////
                    MEMORY-BASED RLP DECODING HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Decode an RLP list header from memory
    function _decodeListHeaderMem(
        bytes memory data,
        uint256 ptr
    )
        internal
        pure
        returns (uint256 listLen, uint256 offset)
    {
        if (ptr >= data.length) return (0, 0);

        uint8 prefix = uint8(data[ptr]);

        if (prefix <= 0xbf) {
            // Not a list
            return (0, 0);
        } else if (prefix <= 0xf7) {
            // Short list: 0xc0 + length
            listLen = prefix - 0xc0;
            offset = ptr + 1;
            if (offset + listLen > data.length) revert InvalidRlpData();
        } else {
            // Long list: 0xf7 + length of length
            uint256 lenLen = prefix - 0xf7;
            listLen = _decodeRlpLongPayloadLength(data, ptr, lenLen);
            offset = ptr + 1 + lenLen;
            if (offset + listLen > data.length) revert InvalidRlpData();
        }
    }

    function _skipRlpItemInList(
        bytes memory data,
        uint256 ptr,
        uint256 listEnd
    )
        internal
        pure
        returns (uint256 nextPtr)
    {
        if (ptr >= listEnd) revert InvalidRlpData();
        (, nextPtr) = _skipRlpItemMem(data, ptr);
        if (nextPtr > listEnd) revert InvalidRlpData();
    }

    /// @notice Skip an RLP item in memory and return next position
    function _skipRlpItemMem(
        bytes memory data,
        uint256 ptr
    )
        internal
        pure
        returns (uint256 itemLen, uint256 nextPtr)
    {
        if (ptr >= data.length) revert InvalidRlpData();

        uint8 prefix = uint8(data[ptr]);

        if (prefix <= 0x7f) {
            return (1, ptr + 1);
        } else if (prefix <= 0xb7) {
            uint256 strLen = prefix - 0x80;
            if (ptr + 1 + strLen > data.length) revert InvalidRlpData();
            if (strLen == 1 && uint8(data[ptr + 1]) < 0x80) revert InvalidRlpData();
            return (1 + strLen, ptr + 1 + strLen);
        } else if (prefix <= 0xbf) {
            uint256 lenLen = prefix - 0xb7;
            uint256 strLen = _decodeRlpLongPayloadLength(data, ptr, lenLen);
            if (ptr + 1 + lenLen + strLen > data.length) revert InvalidRlpData();
            return (1 + lenLen + strLen, ptr + 1 + lenLen + strLen);
        } else if (prefix <= 0xf7) {
            uint256 listLen = prefix - 0xc0;
            if (ptr + 1 + listLen > data.length) revert InvalidRlpData();
            return (1 + listLen, ptr + 1 + listLen);
        } else {
            uint256 lenLen = prefix - 0xf7;
            uint256 listLen = _decodeRlpLongPayloadLength(data, ptr, lenLen);
            if (ptr + 1 + lenLen + listLen > data.length) revert InvalidRlpData();
            return (1 + lenLen + listLen, ptr + 1 + lenLen + listLen);
        }
    }

    function _decodeRlpLongPayloadLength(
        bytes memory data,
        uint256 ptr,
        uint256 lenLen
    )
        internal
        pure
        returns (uint256 payloadLen)
    {
        if (ptr + 1 + lenLen > data.length) revert InvalidRlpData();
        if (uint8(data[ptr + 1]) == 0) revert InvalidRlpData();

        for (uint256 i = 0; i < lenLen; i++) {
            payloadLen = (payloadLen << 8) | uint8(data[ptr + 1 + i]);
        }
        if (payloadLen < 56) revert InvalidRlpData();
    }

    /// @notice Decode a bytes32 from RLP in memory
    function _decodeBytes32Mem(
        bytes memory data,
        uint256 ptr
    )
        internal
        pure
        returns (bytes32 value)
    {
        if (ptr >= data.length) revert InvalidRlpData();

        uint8 prefix = uint8(data[ptr]);

        if (prefix == 0xa0) {
            // 32-byte string: 0x80 + 32 = 0xa0
            if (ptr + 33 > data.length) revert InvalidRlpData();
            assembly {
                value := mload(add(add(data, 0x20), add(ptr, 1)))
            }
        } else {
            revert InvalidRlpData();
        }
    }

    /// @notice Decode a uint64 from RLP in memory
    function _decodeUint64Mem(bytes memory data, uint256 ptr) internal pure returns (uint64 value) {
        if (ptr >= data.length) revert InvalidRlpData();

        uint8 prefix = uint8(data[ptr]);

        if (prefix <= 0x7f) {
            return uint64(prefix);
        } else if (prefix == 0x80) {
            return 0;
        } else if (prefix >= 0x81 && prefix <= 0x88) {
            uint256 strLen = prefix - 0x80;
            if (ptr + 1 + strLen > data.length) revert InvalidRlpData();
            if (uint8(data[ptr + 1]) == 0) revert InvalidRlpData();
            if (strLen == 1 && uint8(data[ptr + 1]) < 0x80) revert InvalidRlpData();

            value = 0;
            for (uint256 i = 0; i < strLen; i++) {
                value = (value << 8) | uint64(uint8(data[ptr + 1 + i]));
            }
        } else {
            revert InvalidRlpData();
        }
    }

    /// @notice Decode a uint256 from RLP in memory
    function _decodeUint256Mem(
        bytes memory data,
        uint256 ptr
    )
        internal
        pure
        returns (uint256 value)
    {
        if (ptr >= data.length) revert InvalidRlpData();

        uint8 prefix = uint8(data[ptr]);

        if (prefix <= 0x7f) {
            return uint256(prefix);
        } else if (prefix == 0x80) {
            return 0;
        } else if (prefix >= 0x81 && prefix <= 0xa0) {
            uint256 strLen = prefix - 0x80;
            if (ptr + 1 + strLen > data.length) revert InvalidRlpData();
            if (uint8(data[ptr + 1]) == 0) revert InvalidRlpData();
            if (strLen == 1 && uint8(data[ptr + 1]) < 0x80) revert InvalidRlpData();

            value = 0;
            for (uint256 i = 0; i < strLen; i++) {
                value = (value << 8) | uint256(uint8(data[ptr + 1 + i]));
            }
        } else {
            revert InvalidRlpData();
        }
    }

    /// @notice Decode an address from RLP in memory
    function _decodeAddressMem(
        bytes memory data,
        uint256 ptr
    )
        internal
        pure
        returns (address value)
    {
        if (ptr >= data.length) revert InvalidRlpData();

        uint8 prefix = uint8(data[ptr]);

        if (prefix == 0x94) {
            // 20-byte string: 0x80 + 20 = 0x94
            if (ptr + 21 > data.length) revert InvalidRlpData();
            assembly {
                value := shr(96, mload(add(add(data, 0x20), add(ptr, 1))))
            }
        } else {
            revert InvalidRlpData();
        }
    }

}
