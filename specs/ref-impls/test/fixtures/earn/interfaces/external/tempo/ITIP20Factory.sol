// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface ITIP20Factory {

    function createToken(
        string calldata name,
        string calldata symbol,
        string calldata currency,
        address quoteToken,
        address admin,
        bytes32 salt
    )
        external
        returns (address token);

    function getTokenAddress(address sender, bytes32 salt) external view returns (address token);

}

interface ITIP20IssuerAccess {

    function ISSUER_ROLE() external view returns (bytes32);
    function grantRole(bytes32 role, address account) external;
    function hasRole(address account, bytes32 role) external view returns (bool);
    function revokeRole(bytes32 role, address account) external;
    function totalSupply() external view returns (uint256);

}

interface ITIP20Metadata {

    function currency() external view returns (string memory);

}
