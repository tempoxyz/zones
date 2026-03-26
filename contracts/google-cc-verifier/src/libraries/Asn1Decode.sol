// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {LibBytes} from "./LibBytes.sol";
import {DateTimeLib} from "solady/utils/DateTimeLib.sol";

type Asn1Ptr is uint256;

/// @dev Packed node descriptor for a single DER TLV.
/// - bits [0..79] store the tag/header offset.
/// - bits [80..159] store the first content byte offset.
/// - bits [160..239] store the content length in bytes.
library LibAsn1Ptr {
    function headerOffset(Asn1Ptr self) internal pure returns (uint256) {
        return uint80(Asn1Ptr.unwrap(self));
    }

    function contentOffset(Asn1Ptr self) internal pure returns (uint256) {
        return uint80(Asn1Ptr.unwrap(self) >> 80);
    }

    function contentLength(Asn1Ptr self) internal pure returns (uint256) {
        return uint80(Asn1Ptr.unwrap(self) >> 160);
    }

    function encodedLength(Asn1Ptr self) internal pure returns (uint256) {
        return LibAsn1Ptr.contentLength(self) + LibAsn1Ptr.contentOffset(self) - LibAsn1Ptr.headerOffset(self);
    }

    function fromOffsets(uint256 headerOffset_, uint256 contentOffset_, uint256 contentLength_)
        internal
        pure
        returns (Asn1Ptr)
    {
        return Asn1Ptr.wrap(headerOffset_ | contentOffset_ << 80 | contentLength_ << 160);
    }
}

library Asn1Decode {
    using LibAsn1Ptr for Asn1Ptr;
    using LibBytes for bytes;

    function root(bytes memory der) internal pure returns (Asn1Ptr) {
        return _readNodeLength(der, 0);
    }

    function rootOf(bytes memory der, Asn1Ptr ptr) internal pure returns (Asn1Ptr) {
        return _readNodeLength(der, ptr.contentOffset());
    }

    function nextSiblingOf(bytes memory der, Asn1Ptr ptr) internal pure returns (Asn1Ptr) {
        return _readNodeLength(der, ptr.contentOffset() + ptr.contentLength());
    }

    function firstChildOf(bytes memory der, Asn1Ptr ptr) internal pure returns (Asn1Ptr) {
        require(der[ptr.headerOffset()] & 0x20 == 0x20, "not constructed");
        return _readNodeLength(der, ptr.contentOffset());
    }

    function bitstring(bytes memory der, Asn1Ptr ptr) internal pure returns (Asn1Ptr) {
        require(der[ptr.headerOffset()] == 0x03, "not bit string");
        require(der[ptr.contentOffset()] == 0x00, "non zero padded bit string");
        return LibAsn1Ptr.fromOffsets(ptr.headerOffset(), ptr.contentOffset() + 1, ptr.contentLength() - 1);
    }

    function octetString(bytes memory der, Asn1Ptr ptr) internal pure returns (Asn1Ptr) {
        require(der[ptr.headerOffset()] == 0x04, "not octet string");
        return _readNodeLength(der, ptr.contentOffset());
    }

    function uintAt(bytes memory der, Asn1Ptr ptr) internal pure returns (uint256) {
        require(der[ptr.headerOffset()] == 0x02, "not integer");
        require(der[ptr.contentOffset()] & 0x80 == 0, "negative integer");
        uint256 contentLength = ptr.contentLength();
        return uint256(_readBytesN(der, ptr.contentOffset(), contentLength) >> (32 - contentLength) * 8);
    }

    function timestampAt(bytes memory der, Asn1Ptr ptr) internal pure returns (uint256) {
        uint8 nodeType = uint8(der[ptr.headerOffset()]);
        uint256 contentOffset = ptr.contentOffset();
        uint256 contentLength = ptr.contentLength();

        require(
            (nodeType == 0x17 && contentLength == 13) || (nodeType == 0x18 && contentLength == 15),
            "invalid timestamp"
        );
        require(der[contentOffset + contentLength - 1] == 0x5A, "timestamp not utc");
        for (uint256 i = 0; i < contentLength - 1; i++) {
            uint8 v = uint8(der[contentOffset + i]);
            require(48 <= v && v <= 57, "invalid timestamp char");
        }

        uint16 yearValue;
        if (contentLength == 13) {
            yearValue = (uint8(der[contentOffset]) - 48 < 5) ? 2000 : 1900;
        } else {
            yearValue = (uint8(der[contentOffset]) - 48) * 1000 + (uint8(der[contentOffset + 1]) - 48) * 100;
            contentOffset += 2;
        }

        yearValue += (uint8(der[contentOffset]) - 48) * 10 + uint8(der[contentOffset + 1]) - 48;
        uint8 monthValue = (uint8(der[contentOffset + 2]) - 48) * 10 + uint8(der[contentOffset + 3]) - 48;
        uint8 dayValue = (uint8(der[contentOffset + 4]) - 48) * 10 + uint8(der[contentOffset + 5]) - 48;
        uint8 hourValue = (uint8(der[contentOffset + 6]) - 48) * 10 + uint8(der[contentOffset + 7]) - 48;
        uint8 minuteValue = (uint8(der[contentOffset + 8]) - 48) * 10 + uint8(der[contentOffset + 9]) - 48;
        uint8 secondValue = (uint8(der[contentOffset + 10]) - 48) * 10 + uint8(der[contentOffset + 11]) - 48;

        return _timestampFromDateTime(yearValue, monthValue, dayValue, hourValue, minuteValue, secondValue);
    }

    function _readNodeLength(bytes memory der, uint256 index) private pure returns (Asn1Ptr) {
        require(der[index] & 0x1f != 0x1f, "long tags unsupported");
        uint256 length;
        uint256 contentOffset;

        if ((der[index + 1] & 0x80) == 0) {
            length = uint8(der[index + 1]);
            contentOffset = index + 2;
        } else {
            uint8 lengthBytesLength = uint8(der[index + 1] & 0x7F);
            if (lengthBytesLength == 1) {
                length = uint8(der[index + 2]);
            } else if (lengthBytesLength == 2) {
                length = der.readUint16(index + 2);
            } else {
                length = uint256(_readBytesN(der, index + 2, lengthBytesLength) >> (32 - lengthBytesLength) * 8);
                require(length <= type(uint64).max, "asn1 length too large");
            }
            contentOffset = index + 2 + lengthBytesLength;
        }

        return LibAsn1Ptr.fromOffsets(index, contentOffset, length);
    }

    function _readBytesN(bytes memory self, uint256 index, uint256 len) private pure returns (bytes32 ret) {
        require(len <= 32, "read too long");
        require(index + len <= self.length, "index out of bounds");
        if (len == 0) {
            return bytes32(0);
        }

        uint256 trailingBytes = 32 - len;
        uint256 lowMask = trailingBytes == 0 ? 0 : (uint256(1) << (trailingBytes * 8)) - 1;
        return self.load(index) & ~bytes32(lowMask);
    }

    function _timestampFromDateTime(
        uint256 year,
        uint256 month,
        uint256 day,
        uint256 hour,
        uint256 minute,
        uint256 second
    ) private pure returns (uint256) {
        require(DateTimeLib.isSupportedDateTime(year, month, day, hour, minute, second), "invalid timestamp");
        return DateTimeLib.dateTimeToTimestamp(year, month, day, hour, minute, second);
    }
}
