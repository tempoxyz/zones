// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Small non-upgradeable ERC-1967 proxy used only to initialize the copied VaultAdapter.
contract TestERC1967Proxy {

    bytes32 private constant IMPLEMENTATION_SLOT =
        0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc;

    constructor(address implementation, bytes memory initialization) payable {
        bytes32 slot = IMPLEMENTATION_SLOT;
        assembly ("memory-safe") {
            sstore(slot, implementation)
        }

        if (initialization.length != 0) {
            (bool success, bytes memory result) = implementation.delegatecall(initialization);
            if (!success) {
                assembly ("memory-safe") {
                    revert(add(result, 0x20), mload(result))
                }
            }
        }
    }

    fallback() external payable {
        _delegate();
    }

    receive() external payable {
        _delegate();
    }

    function _delegate() private {
        bytes32 slot = IMPLEMENTATION_SLOT;
        assembly ("memory-safe") {
            let implementation := sload(slot)
            calldatacopy(0, 0, calldatasize())
            let success := delegatecall(gas(), implementation, 0, calldatasize(), 0, 0)
            returndatacopy(0, 0, returndatasize())
            switch success
            case 0 { revert(0, returndatasize()) }
            default { return(0, returndatasize()) }
        }
    }

}
