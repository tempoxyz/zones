// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

// Protocol-managed TIP-1091 contracts shared by every Tempo Zone.
address constant ZONE_FACTORY_ADDRESS = 0x5aF2000000000000000000000000000000000000;
address constant ZONE_MESSENGER_ADDRESS = 0x5A4d000000000000000000000000000000000000;

struct ZoneInfo {
    uint32 zoneId;
    address portal;
    bool accessMode;
    bool gatewayMode;
    address admin;
    address[] sequencers;
    uint8 threshold;
    address verifier;
    string rpcUrl;
}

struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;
    uint8 ephemeralPubkeyYParity;
    bytes ciphertext;
    bytes12 nonce;
    bytes16 tag;
}

interface IZoneFactory {

    function zones(uint32 zoneId) external view returns (ZoneInfo memory info);
    function isZonePortal(address portal) external view returns (bool);

}

interface IZonePortal {

    function isTokenEnabled(address token) external view returns (bool);
    function areDepositsActive(address token) external view returns (bool);

    function depositEncrypted(
        address token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted,
        address tempoRefundRecipient
    )
        external
        returns (bytes32 newCurrentDepositQueueHash);

}

interface IWithdrawalReceiver {

    function onWithdrawalReceived(
        uint32 zoneId,
        address sourcePortal,
        bytes32 senderTag,
        address token,
        uint128 amount,
        bytes calldata callbackData
    )
        external
        returns (bytes4);

}
