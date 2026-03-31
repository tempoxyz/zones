// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITempoState, ZONE_INBOX } from "../../src/zone/IZone.sol";
import { TempoState } from "../../src/zone/TempoState.sol";
import { Test } from "forge-std/Test.sol";

/// @title TempoStateTest
/// @notice Tests for the TempoState predeploy contract
contract TempoStateTest is Test {

    TempoState public tempoState;

    address public zoneInbox = ZONE_INBOX;
    address public notZoneInbox = address(0x2);

    // Genesis values - we'll use a real encoded header
    uint64 constant GENESIS_BLOCK_NUMBER = 100;
    uint64 constant GENESIS_TIMESTAMP = 1_700_000_000;
    bytes32 constant GENESIS_STATE_ROOT = keccak256("genesisStateRoot");
    bytes32 constant GENESIS_RECEIPTS_ROOT = keccak256("genesisReceiptsRoot");
    bytes32 constant GENESIS_TX_ROOT = keccak256("genesisTxRoot");
    bytes32 constant GENESIS_PARENT_HASH = keccak256("genesisParent");
    address constant GENESIS_BENEFICIARY = address(0xBEEF);

    bytes public genesisHeader;
    bytes32 public genesisBlockHash;

    function setUp() public {
        // Build genesis header
        genesisHeader = _buildTempoHeader(
            GENESIS_PARENT_HASH,
            GENESIS_STATE_ROOT,
            GENESIS_RECEIPTS_ROOT,
            GENESIS_TX_ROOT,
            GENESIS_BENEFICIARY,
            GENESIS_BLOCK_NUMBER,
            GENESIS_TIMESTAMP
        );
        genesisBlockHash = keccak256(genesisHeader);

        tempoState = new TempoState(genesisHeader);
    }

    /*//////////////////////////////////////////////////////////////
                          CONSTRUCTOR TESTS
    //////////////////////////////////////////////////////////////*/

    function test_constructor_initializesState() public view {
        assertEq(tempoState.tempoBlockHash(), genesisBlockHash);
        assertEq(tempoState.tempoBlockNumber(), GENESIS_BLOCK_NUMBER);
        assertEq(tempoState.tempoTimestamp(), GENESIS_TIMESTAMP);
        assertEq(tempoState.tempoStateRoot(), GENESIS_STATE_ROOT);
        assertEq(tempoState.tempoReceiptsRoot(), GENESIS_RECEIPTS_ROOT);
        assertEq(tempoState.tempoTransactionsRoot(), GENESIS_TX_ROOT);
        assertEq(tempoState.tempoParentHash(), GENESIS_PARENT_HASH);
        assertEq(tempoState.tempoBeneficiary(), GENESIS_BENEFICIARY);
    }

    /*//////////////////////////////////////////////////////////////
                       FINALIZE TEMPO TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeTempo_updatesState() public {
        bytes32 newStateRoot = keccak256("newStateRoot");
        bytes32 newReceiptsRoot = keccak256("newReceiptsRoot");
        bytes32 newTxRoot = keccak256("newTxRoot");
        address newBeneficiary = address(0xCAFE);

        // Build a valid Tempo header that references the genesis block
        bytes memory header = _buildTempoHeader(
            genesisBlockHash, // parentHash
            newStateRoot,
            newReceiptsRoot,
            newTxRoot,
            newBeneficiary,
            GENESIS_BLOCK_NUMBER + 1,
            GENESIS_TIMESTAMP + 12
        );

        vm.prank(zoneInbox);
        tempoState.finalizeTempo(header);

        // Verify state was updated
        assertEq(tempoState.tempoBlockHash(), keccak256(header));
        assertEq(tempoState.tempoBlockNumber(), GENESIS_BLOCK_NUMBER + 1);
        assertEq(tempoState.tempoTimestamp(), GENESIS_TIMESTAMP + 12);
        assertEq(tempoState.tempoStateRoot(), newStateRoot);
        assertEq(tempoState.tempoReceiptsRoot(), newReceiptsRoot);
        assertEq(tempoState.tempoTransactionsRoot(), newTxRoot);
        assertEq(tempoState.tempoParentHash(), genesisBlockHash);
        assertEq(tempoState.tempoBeneficiary(), newBeneficiary);
    }

    function test_finalizeTempo_multipleBlocks() public {
        // Finalize block 101
        bytes memory header1 = _buildTempoHeader(
            genesisBlockHash,
            keccak256("stateRoot1"),
            keccak256("receiptsRoot1"),
            keccak256("txRoot1"),
            address(0x1),
            GENESIS_BLOCK_NUMBER + 1,
            GENESIS_TIMESTAMP + 12
        );

        vm.prank(zoneInbox);
        tempoState.finalizeTempo(header1);

        bytes32 block101Hash = keccak256(header1);
        assertEq(tempoState.tempoBlockHash(), block101Hash);
        assertEq(tempoState.tempoBlockNumber(), GENESIS_BLOCK_NUMBER + 1);

        // Finalize block 102
        bytes memory header2 = _buildTempoHeader(
            block101Hash, // parentHash = block 101's hash
            keccak256("stateRoot2"),
            keccak256("receiptsRoot2"),
            keccak256("txRoot2"),
            address(0x2),
            GENESIS_BLOCK_NUMBER + 2,
            GENESIS_TIMESTAMP + 24
        );

        vm.prank(zoneInbox);
        tempoState.finalizeTempo(header2);

        bytes32 block102Hash = keccak256(header2);
        assertEq(tempoState.tempoBlockHash(), block102Hash);
        assertEq(tempoState.tempoBlockNumber(), GENESIS_BLOCK_NUMBER + 2);
        assertEq(tempoState.tempoTimestamp(), GENESIS_TIMESTAMP + 24);
    }

    function test_finalizeTempo_revertsOnInvalidParentHash() public {
        bytes memory header = _buildTempoHeader(
            keccak256("wrongParent"), // Invalid parent hash
            keccak256("stateRoot"),
            keccak256("receiptsRoot"),
            keccak256("txRoot"),
            address(0x1),
            GENESIS_BLOCK_NUMBER + 1,
            GENESIS_TIMESTAMP + 12
        );

        vm.prank(zoneInbox);
        vm.expectRevert(ITempoState.InvalidParentHash.selector);
        tempoState.finalizeTempo(header);
    }

    function test_finalizeTempo_revertsOnInvalidBlockNumber() public {
        bytes memory header = _buildTempoHeader(
            genesisBlockHash,
            keccak256("stateRoot"),
            keccak256("receiptsRoot"),
            keccak256("txRoot"),
            address(0x1),
            GENESIS_BLOCK_NUMBER + 2, // Should be +1
            GENESIS_TIMESTAMP + 12
        );

        vm.prank(zoneInbox);
        vm.expectRevert(ITempoState.InvalidBlockNumber.selector);
        tempoState.finalizeTempo(header);
    }

    function test_finalizeTempo_revertsOnSkippedBlockNumber() public {
        bytes memory header = _buildTempoHeader(
            genesisBlockHash,
            keccak256("stateRoot"),
            keccak256("receiptsRoot"),
            keccak256("txRoot"),
            address(0x1),
            GENESIS_BLOCK_NUMBER, // Same as current, not +1
            GENESIS_TIMESTAMP + 12
        );

        vm.prank(zoneInbox);
        vm.expectRevert(ITempoState.InvalidBlockNumber.selector);
        tempoState.finalizeTempo(header);
    }

    function test_finalizeTempo_revertsIfNotZoneInbox() public {
        bytes memory header = _buildTempoHeader(
            genesisBlockHash,
            keccak256("stateRoot"),
            keccak256("receiptsRoot"),
            keccak256("txRoot"),
            address(0x1),
            GENESIS_BLOCK_NUMBER + 1,
            GENESIS_TIMESTAMP + 12
        );

        vm.prank(notZoneInbox);
        vm.expectRevert(ITempoState.OnlyZoneInbox.selector);
        tempoState.finalizeTempo(header);
    }

    function test_finalizeTempo_emitsEvent() public {
        bytes32 newStateRoot = keccak256("stateRoot");
        bytes memory header = _buildTempoHeader(
            genesisBlockHash,
            newStateRoot,
            keccak256("receiptsRoot"),
            keccak256("txRoot"),
            address(0x1),
            GENESIS_BLOCK_NUMBER + 1,
            GENESIS_TIMESTAMP + 12
        );

        vm.prank(zoneInbox);
        vm.expectEmit(true, true, false, true);
        emit ITempoState.TempoBlockFinalized(
            keccak256(header), GENESIS_BLOCK_NUMBER + 1, newStateRoot
        );
        tempoState.finalizeTempo(header);
    }

    /*//////////////////////////////////////////////////////////////
                        STORAGE READING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_readTempoStorageSlot_revertsWithoutPrecompile() public {
        vm.prank(zoneInbox);
        vm.expectRevert();
        tempoState.readTempoStorageSlot(address(0x1234), bytes32(0));
    }

    function test_readTempoStorageSlots_revertsWithoutPrecompile() public {
        bytes32[] memory slots = new bytes32[](2);
        slots[0] = bytes32(uint256(1));
        slots[1] = bytes32(uint256(2));

        vm.prank(zoneInbox);
        vm.expectRevert();
        tempoState.readTempoStorageSlots(address(0x1234), slots);
    }

    /*//////////////////////////////////////////////////////////////
                          RLP ENCODING HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Build a minimal valid Tempo header for testing
    /// @dev Tempo header: rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])
    ///      Inner: standard Ethereum header with fields we care about
    function _buildTempoHeader(
        bytes32 parentHash,
        bytes32 stateRoot,
        bytes32 receiptsRoot,
        bytes32 transactionsRoot,
        address beneficiary,
        uint64 number,
        uint64 timestamp
    )
        internal
        pure
        returns (bytes memory)
    {
        // Build inner Ethereum header RLP
        bytes memory innerHeader = _encodeEthereumHeader(
            parentHash, stateRoot, receiptsRoot, transactionsRoot, beneficiary, number, timestamp
        );

        // Build outer Tempo header: [general_gas_limit, shared_gas_limit, timestamp_millis_part, inner]
        bytes memory outerContent = abi.encodePacked(
            _encodeUint64(30_000_000), // general_gas_limit
            _encodeUint64(10_000_000), // shared_gas_limit
            _encodeUint64(0), // timestamp_millis_part
            innerHeader
        );

        return _encodeList(outerContent);
    }

    /// @notice Encode a minimal Ethereum header with the fields we need
    function _encodeEthereumHeader(
        bytes32 parentHash,
        bytes32 stateRoot,
        bytes32 receiptsRoot,
        bytes32 transactionsRoot,
        address beneficiary,
        uint64 number,
        uint64 timestamp
    )
        internal
        pure
        returns (bytes memory)
    {
        // Standard Ethereum header fields (in order):
        // 0: parentHash, 1: ommersHash, 2: beneficiary, 3: stateRoot,
        // 4: transactionsRoot, 5: receiptsRoot, 6: logsBloom, 7: difficulty,
        // 8: number, 9: gasLimit, 10: gasUsed, 11: timestamp, 12+: extra fields

        bytes memory content = abi.encodePacked(
            _encodeBytes32(parentHash), // 0: parentHash
            _encodeBytes32(keccak256("ommersHash")), // 1: ommersHash
            _encodeAddress(beneficiary), // 2: beneficiary
            _encodeBytes32(stateRoot), // 3: stateRoot
            _encodeBytes32(transactionsRoot), // 4: transactionsRoot
            _encodeBytes32(receiptsRoot), // 5: receiptsRoot
            _encodeBloom(), // 6: logsBloom (256 bytes)
            _encodeUint64(0), // 7: difficulty
            _encodeUint64(number), // 8: number
            _encodeUint64(30_000_000), // 9: gasLimit
            _encodeUint64(0), // 10: gasUsed
            _encodeUint64(timestamp), // 11: timestamp
            _encodeBytes(bytes("")), // 12: extraData
            _encodeBytes32(bytes32(0)), // 13: mixHash
            _encodeBytes8(bytes8(0)), // 14: nonce
            _encodeUint64(1_000_000_000), // 15: baseFeePerGas
            _encodeBytes32(bytes32(0)), // 16: withdrawalsRoot
            _encodeUint64(0), // 17: blobGasUsed
            _encodeUint64(0) // 18: excessBlobGas
        );

        return _encodeList(content);
    }

    function _encodeBytes32(bytes32 value) internal pure returns (bytes memory) {
        return abi.encodePacked(bytes1(0xa0), value);
    }

    function _encodeAddress(address value) internal pure returns (bytes memory) {
        return abi.encodePacked(bytes1(0x94), value);
    }

    function _encodeUint64(uint64 value) internal pure returns (bytes memory) {
        if (value == 0) {
            return abi.encodePacked(bytes1(0x80));
        } else if (value < 128) {
            return abi.encodePacked(bytes1(uint8(value)));
        } else {
            // Count bytes needed
            uint256 temp = value;
            uint8 byteLen = 0;
            while (temp > 0) {
                byteLen++;
                temp >>= 8;
            }
            bytes memory result = new bytes(1 + byteLen);
            result[0] = bytes1(0x80 + byteLen);
            for (uint8 i = 0; i < byteLen; i++) {
                result[byteLen - i] = bytes1(uint8(value >> (8 * i)));
            }
            return result;
        }
    }

    function _encodeBytes8(bytes8 value) internal pure returns (bytes memory) {
        return abi.encodePacked(bytes1(0x88), value);
    }

    function _encodeBytes(bytes memory value) internal pure returns (bytes memory) {
        if (value.length == 0) {
            return abi.encodePacked(bytes1(0x80));
        } else if (value.length == 1 && uint8(value[0]) < 128) {
            return value;
        } else if (value.length <= 55) {
            return abi.encodePacked(bytes1(uint8(0x80 + value.length)), value);
        } else {
            revert("Long string encoding not implemented");
        }
    }

    function _encodeBloom() internal pure returns (bytes memory) {
        // 256-byte bloom filter: 0xb9 0x01 0x00 + 256 zero bytes
        bytes memory bloom = new bytes(256);
        bytes memory prefix = abi.encodePacked(bytes1(0xb9), bytes1(0x01), bytes1(0x00));
        return abi.encodePacked(prefix, bloom);
    }

    function _encodeList(bytes memory content) internal pure returns (bytes memory) {
        if (content.length <= 55) {
            return abi.encodePacked(bytes1(uint8(0xc0 + content.length)), content);
        } else {
            // Long list
            uint256 lenLen = _bytesNeeded(content.length);
            bytes memory result = new bytes(1 + lenLen + content.length);
            result[0] = bytes1(uint8(0xf7 + lenLen));
            for (uint256 i = 0; i < lenLen; i++) {
                result[lenLen - i] = bytes1(uint8(content.length >> (8 * i)));
            }
            for (uint256 i = 0; i < content.length; i++) {
                result[1 + lenLen + i] = content[i];
            }
            return result;
        }
    }

    function _bytesNeeded(uint256 value) internal pure returns (uint256) {
        uint256 count = 0;
        while (value > 0) {
            count++;
            value >>= 8;
        }
        return count == 0 ? 1 : count;
    }

}
