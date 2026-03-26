// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IGooglePkiAttestationVerifier} from "../interfaces/IGooglePkiAttestationVerifier.sol";
import {JSONParserLib} from "solady/utils/JSONParserLib.sol";

library GoogleAttestationPayloadParser {
    using JSONParserLib for JSONParserLib.Item;

    // JSONParserLib returns object keys as raw JSON tokens, so object lookups use quoted keys.
    string internal constant ISS_KEY = "\"iss\"";
    string internal constant EAT_NONCE_KEY = "\"eat_nonce\"";
    string internal constant IAT_KEY = "\"iat\"";
    string internal constant NBF_KEY = "\"nbf\"";
    string internal constant EXP_KEY = "\"exp\"";
    string internal constant HWMODEL_KEY = "\"hwmodel\"";
    string internal constant DBGSTAT_KEY = "\"dbgstat\"";
    string internal constant SUBMODS_KEY = "\"submods\"";
    string internal constant CONTAINER_KEY = "\"container\"";
    string internal constant IMAGE_DIGEST_KEY = "\"image_digest\"";

    function parseRequiredClaims(bytes memory payloadJson)
        internal
        pure
        returns (IGooglePkiAttestationVerifier.Claims memory claims)
    {
        // Intentionally narrow parser: only the claims enforced by the validator are extracted.
        // Required keys must be unique so the policy never depends on ambiguous duplicate-key JSON.
        JSONParserLib.Item memory root = JSONParserLib.parse(string(payloadJson));
        require(root.isObject(), "payload must be object");

        claims.issuerHash = _extractStringHash(root, ISS_KEY);
        claims.eatNonce = _extractBytes32HexOrSingleStringArray(root, EAT_NONCE_KEY);
        claims.issuedAt = _extractUint64(root, IAT_KEY);
        claims.notBefore = _extractUint64(root, NBF_KEY);
        claims.expiresAt = _extractUint64(root, EXP_KEY);
        claims.hwModelHash = _extractStringHash(root, HWMODEL_KEY);
        claims.debugStateHash = _extractStringHash(root, DBGSTAT_KEY);

        JSONParserLib.Item memory submods = _extractObject(root, SUBMODS_KEY);
        JSONParserLib.Item memory container = _extractObject(submods, CONTAINER_KEY);
        claims.imageDigestHash = _extractStringHash(container, IMAGE_DIGEST_KEY);
    }

    function _extractStringHash(JSONParserLib.Item memory objectItem, string memory key) private pure returns (bytes32) {
        JSONParserLib.Item memory item = _requireUniqueField(objectItem, key);
        require(item.isString(), "string claim expected");
        return keccak256(bytes(JSONParserLib.decodeString(item.value())));
    }

    function _extractBytes32HexOrSingleStringArray(JSONParserLib.Item memory objectItem, string memory key)
        private
        pure
        returns (bytes32)
    {
        JSONParserLib.Item memory item = _requireUniqueField(objectItem, key);

        if (item.isString()) {
            return _decodeBytes32Hex(JSONParserLib.decodeString(item.value()));
        }
        if (item.isArray()) {
            return _readSingletonStringArrayBytes32(item);
        }

        revert("unsupported claim type");
    }

    function _extractUint64(JSONParserLib.Item memory objectItem, string memory key) private pure returns (uint64) {
        JSONParserLib.Item memory item = _requireUniqueField(objectItem, key);
        require(item.isNumber(), "uint claim expected");

        uint256 value = JSONParserLib.parseUint(item.value());
        require(value <= type(uint64).max, "uint overflow");
        return uint64(value);
    }

    function _extractObject(JSONParserLib.Item memory objectItem, string memory key)
        private
        pure
        returns (JSONParserLib.Item memory)
    {
        JSONParserLib.Item memory item = _requireUniqueField(objectItem, key);
        require(item.isObject(), "object claim expected");
        return item;
    }

    function _requireUniqueField(JSONParserLib.Item memory objectItem, string memory key)
        private
        pure
        returns (JSONParserLib.Item memory result)
    {
        require(objectItem.isObject(), "object claim expected");

        JSONParserLib.Item[] memory children = objectItem.children();
        bytes32 keyHash = keccak256(bytes(key));
        uint256 matches;

        for (uint256 i = 0; i < children.length; ++i) {
            if (keccak256(bytes(children[i].key())) != keyHash) {
                continue;
            }

            matches++;
            require(matches == 1, "duplicate claim");
            result = children[i];
        }

        require(matches == 1, "missing claim");
    }

    function _readSingletonStringArrayBytes32(JSONParserLib.Item memory arrayItem) private pure returns (bytes32) {
        require(arrayItem.isArray(), "array expected");
        // Google can encode `eat_nonce` as either a single string or a singleton array.
        require(arrayItem.size() == 1, "multiple array values unsupported");

        JSONParserLib.Item memory item = arrayItem.at(0);
        require(item.isString(), "single string array expected");
        return _decodeBytes32Hex(JSONParserLib.decodeString(item.value()));
    }

    function _decodeBytes32Hex(string memory asciiHexString) private pure returns (bytes32) {
        bytes memory asciiHex = bytes(asciiHexString);
        if (asciiHex.length == 66) {
            require(asciiHex[0] == "0" && asciiHex[1] == "x", "hex prefix required");
        } else {
            require(asciiHex.length == 64, "bytes32 hex length invalid");
        }

        return bytes32(JSONParserLib.parseUintFromHex(asciiHexString));
    }
}
