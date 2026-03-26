// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Base64} from "solady/utils/Base64.sol";

library Base64Url {
    function decode(bytes memory input) internal pure returns (bytes memory output) {
        _validate(input);
        return Base64.decode(string(input));
    }

    function _validate(bytes memory input) private pure {
        require(input.length % 4 != 1, "invalid b64url length");

        for (uint256 i = 0; i < input.length; ++i) {
            uint8 charCode = uint8(input[i]);
            bool isValid = (charCode >= 0x41 && charCode <= 0x5A) || (charCode >= 0x61 && charCode <= 0x7A)
                || (charCode >= 0x30 && charCode <= 0x39) || charCode == 0x2D || charCode == 0x5F;
            require(isValid, "invalid b64url char");
        }
    }
}
