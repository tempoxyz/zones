// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITempoState, ITempoStateReader } from "./IZone.sol";

/// @title TempoState
/// @notice Zone-side predeploy for Tempo state verification
/// @dev Deployed at 0x1c00000000000000000000000000000000000000
///      Stores the latest finalized Tempo block info. Sequencer submits Tempo headers
///      which are validated for chain continuity and decoded to update state.
contract TempoState is ITempoState {

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice Current finalized Tempo block hash (keccak256 of RLP-encoded header)
    bytes32 public tempoBlockHash;

    /*//////////////////////////////////////////////////////////////
                          TEMPO WRAPPER FIELDS
    //////////////////////////////////////////////////////////////*/

    /// @notice Tempo general gas limit (outer header field)
    uint64 public generalGasLimit;

    /// @notice Tempo shared gas limit (outer header field)
    uint64 public sharedGasLimit;

    /*//////////////////////////////////////////////////////////////
                        INNER ETHEREUM HEADER FIELDS
    //////////////////////////////////////////////////////////////*/

    /// @notice Parent block hash
    bytes32 public tempoParentHash;

    /// @notice Block producer address
    address public tempoBeneficiary;

    /// @notice State root (for storage proofs)
    bytes32 public tempoStateRoot;

    /// @notice Transactions root (for audit trail)
    bytes32 public tempoTransactionsRoot;

    /// @notice Receipts root
    bytes32 public tempoReceiptsRoot;

    /// @notice Block number
    uint64 public tempoBlockNumber;

    /// @notice Gas limit
    uint64 public tempoGasLimit;

    /// @notice Gas used
    uint64 public tempoGasUsed;

    /// @notice Block timestamp (seconds, combined with millisPart for full precision)
    uint64 public tempoTimestamp;

    /// @notice Millisecond part of timestamp (from Tempo wrapper)
    uint64 public tempoTimestampMillis;

    /// @notice Previous RANDAO value (post-merge mixHash)
    bytes32 public tempoPrevRandao;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    /// @notice Initialize with genesis Tempo block
    /// @param _genesisHeader RLP-encoded genesis Tempo header
    constructor(bytes memory _genesisHeader) {
        // Decode and store genesis header
        _decodeAndStoreHeader(_genesisHeader);
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

        // Store previous values for validation
        bytes32 prevBlockHash = tempoBlockHash;
        uint64 prevBlockNumber = tempoBlockNumber;

        // Decode and store all header fields
        _decodeAndStoreHeader(header);

        // Validate chain continuity
        if (tempoParentHash != prevBlockHash) revert InvalidParentHash();
        if (tempoBlockNumber != prevBlockNumber + 1) revert InvalidBlockNumber();

        emit TempoBlockFinalized(tempoBlockHash, tempoBlockNumber, tempoStateRoot);
    }

    /*//////////////////////////////////////////////////////////////
                    TEMPO STORAGE READING (SYSTEM ONLY)
    //////////////////////////////////////////////////////////////*/

    /// @notice Zone system contract addresses that are allowed to read Tempo state
    /// @dev These contracts need access to read from ZonePortal and TIP-403 on Tempo
    address private constant ZONE_INBOX = 0x1c00000000000000000000000000000000000001;
    address private constant ZONE_OUTBOX = 0x1c00000000000000000000000000000000000002;
    address private constant ZONE_CONFIG = 0x1c00000000000000000000000000000000000003;

    /// @notice TempoStateReader precompile address
    /// @dev Standalone precompile that reads Tempo L1 contract storage at a given block height.
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
        values = ITempoStateReader(TEMPO_STATE_READER).readStorageBatchAt(account, slots, tempoBlockNumber);
    }

    /*//////////////////////////////////////////////////////////////
                          RLP DECODING (INTERNAL)
    //////////////////////////////////////////////////////////////*/

    /// @notice Decode a Tempo header and store fields used by the zone
    /// @dev Tempo header format: rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])
    ///      Inner Ethereum header fields (0-indexed):
    ///        0: parentHash, 1: ommersHash, 2: beneficiary, 3: stateRoot,
    ///        4: transactionsRoot, 5: receiptsRoot, 6: logsBloom, 7: difficulty,
    ///        8: number, 9: gasLimit, 10: gasUsed, 11: timestamp, 12: extraData,
    ///        13: mixHash (prevRandao), 14: nonce, remaining fields are optional and ignored
    function _decodeAndStoreHeader(bytes memory header) internal {
        uint256 ptr = 0;

        // Compute and store block hash
        tempoBlockHash = keccak256(header);

        // Decode outer list header
        (uint256 outerListLen, uint256 outerListOffset) = _decodeListHeaderMem(header, ptr);
        if (outerListOffset == 0) revert InvalidRlpData();
        ptr = outerListOffset;

        // Field 0: general_gas_limit
        generalGasLimit = _decodeUint64Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Field 1: shared_gas_limit
        sharedGasLimit = _decodeUint64Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Field 2: timestamp_millis_part
        tempoTimestampMillis = _decodeUint64Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Field 3: inner Ethereum header (a list)
        (uint256 innerListLen, uint256 innerListOffset) = _decodeListHeaderMem(header, ptr);
        if (innerListOffset == 0) revert InvalidRlpData();
        ptr = innerListOffset;

        // Inner field 0: parentHash
        tempoParentHash = _decodeBytes32Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 1: ommersHash - skip
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 2: beneficiary
        tempoBeneficiary = _decodeAddressMem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 3: stateRoot
        tempoStateRoot = _decodeBytes32Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 4: transactionsRoot
        tempoTransactionsRoot = _decodeBytes32Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 5: receiptsRoot
        tempoReceiptsRoot = _decodeBytes32Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 6: logsBloom - skip
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 7: difficulty - skip
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 8: number
        tempoBlockNumber = _decodeUint64Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 9: gasLimit
        tempoGasLimit = _decodeUint64Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 10: gasUsed
        tempoGasUsed = _decodeUint64Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 11: timestamp
        tempoTimestamp = _decodeUint64Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 12: extraData - skip
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 13: mixHash (prevRandao)
        tempoPrevRandao = _decodeBytes32Mem(header, ptr);
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Inner field 14: nonce - skip
        (, ptr) = _skipRlpItemMem(header, ptr);

        // Skip any remaining optional fields we don't record.
        while (ptr < innerListOffset + innerListLen) {
            (, ptr) = _skipRlpItemMem(header, ptr);
        }

        // Silence unused variable warning
        outerListLen;
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
            return (1 + strLen, ptr + 1 + strLen);
        } else if (prefix <= 0xbf) {
            uint256 lenLen = prefix - 0xb7;
            uint256 strLen = 0;
            for (uint256 i = 0; i < lenLen; i++) {
                strLen = (strLen << 8) | uint8(data[ptr + 1 + i]);
            }
            return (1 + lenLen + strLen, ptr + 1 + lenLen + strLen);
        } else if (prefix <= 0xf7) {
            uint256 listLen = prefix - 0xc0;
            return (1 + listLen, ptr + 1 + listLen);
        } else {
            uint256 lenLen = prefix - 0xf7;
            uint256 listLen = 0;
            for (uint256 i = 0; i < lenLen; i++) {
                listLen = (listLen << 8) | uint8(data[ptr + 1 + i]);
            }
            return (1 + lenLen + listLen, ptr + 1 + lenLen + listLen);
        }
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
        } else if (prefix <= 0x7f) {
            value = bytes32(uint256(prefix));
        } else if (prefix >= 0x80 && prefix <= 0xb7) {
            uint256 strLen = prefix - 0x80;
            if (strLen == 0) {
                value = bytes32(0);
            } else if (strLen <= 32) {
                if (ptr + 1 + strLen > data.length) revert InvalidRlpData();
                assembly {
                    value := mload(add(add(data, 0x20), add(ptr, 1)))
                }
                // Clear extra bytes on the right
                value = value & bytes32(~((1 << (8 * (32 - strLen))) - 1));
            } else {
                revert InvalidRlpData();
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
        } else if (prefix == 0x80) {
            // Empty = zero address
            value = address(0);
        } else {
            revert InvalidRlpData();
        }
    }

}
