// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {LibBytes as SoladyLibBytes} from "solady/utils/LibBytes.sol";

library LibBytes {
    function keccak(bytes memory data, uint256 offset, uint256 length) internal pure returns (bytes32 result) {
        require(offset + length <= data.length, "index out of bounds");
        assembly {
            result := keccak256(add(data, add(32, offset)), length)
        }
    }

    function load(bytes memory b, uint256 index) internal pure returns (bytes32) {
        return SoladyLibBytes.load(b, index);
    }

    function slice(bytes memory b, uint256 offset, uint256 length) internal pure returns (bytes memory result) {
        require(offset + length <= b.length, "index out of bounds");
        return SoladyLibBytes.slice(b, offset, offset + length);
    }

    function readUint16(bytes memory b, uint256 index) internal pure returns (uint16) {
        require(b.length >= index + 2, "index out of bounds");
        return uint16(bytes2(SoladyLibBytes.load(b, index)));
    }
}
