// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface IZonePortal {

    struct EncryptedDepositPayload {
        bytes32 ephemeralPubkeyX;
        uint8 ephemeralPubkeyYParity;
        bytes ciphertext;
        bytes12 nonce;
        bytes16 tag;
    }

    function depositEncrypted(
        address token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted,
        address tempoRefundRecipient
    )
        external
        returns (bytes32);

    function zoneId() external view returns (uint32);

    function messenger() external view returns (address);

}
