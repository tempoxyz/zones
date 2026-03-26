// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IGooglePkiAttestationVerifier} from "./interfaces/IGooglePkiAttestationVerifier.sol";
import {GoogleAttestationPayloadParser} from "./libraries/GoogleAttestationPayloadParser.sol";
import {Asn1Decode, Asn1Ptr, LibAsn1Ptr} from "./libraries/Asn1Decode.sol";
import {LibBytes} from "./libraries/LibBytes.sol";
import {Base64Url} from "./libraries/Base64Url.sol";
import {RSA} from "@openzeppelin/contracts/utils/cryptography/RSA.sol";

contract GooglePkiAttestationVerifier is IGooglePkiAttestationVerifier {
    using Asn1Decode for bytes;
    using LibAsn1Ptr for Asn1Ptr;
    using LibBytes for bytes;

    bytes32 internal constant SHA256_WITH_RSA_ENCRYPTION_ALGO_HASH = keccak256(hex"06092a864886f70d01010b0500");
    bytes32 internal constant RSA_ENCRYPTION_ALGO_HASH = keccak256(hex"06092a864886f70d0101010500");
    bytes32 internal constant BASIC_CONSTRAINTS_OID_HASH = keccak256(hex"551d13");
    bytes32 internal constant KEY_USAGE_OID_HASH = keccak256(hex"551d0f");

    bytes32 public immutable rootSubjectHash;
    uint64 public immutable rootNotAfter;
    bytes private rootExponent;
    bytes private rootModulus;

    /// @dev Normalized certificate data kept after one pass of DER / X.509 parsing.
    struct VerifiedCert {
        bool ca;
        uint64 notAfter;
        bytes32 issuerHash;
        bytes32 subjectHash;
        bytes exponent;
        bytes modulus;
    }

    constructor(bytes memory rootCertificateDer) {
        VerifiedCert memory root = _parseCertificate(rootCertificateDer, true);
        require(root.issuerHash == root.subjectHash, "root issuer mismatch");

        bytes memory signature = _certificateSignature(rootCertificateDer);
        require(
            RSA.pkcs1Sha256(sha256(_tbsCertificateBytes(rootCertificateDer)), signature, root.exponent, root.modulus),
            "root sig invalid"
        );

        rootSubjectHash = root.subjectHash;
        rootNotAfter = root.notAfter;
        rootExponent = root.exponent;
        rootModulus = root.modulus;
    }

    function verifyAndExtract(Proof calldata proof) external view returns (Claims memory) {
        VerifiedCert memory root = _pinnedRootCert();
        require(root.notAfter >= block.timestamp, "root expired");

        VerifiedCert memory intermediate = _verifyCertificate(proof.intermediateCertificateDer, true, root);
        VerifiedCert memory leaf = _verifyCertificate(proof.leafCertificateDer, false, intermediate);

        bytes memory signingInput = proof.signingInput;
        bytes memory signature = proof.signature;
        require(RSA.pkcs1Sha256(signingInput, signature, leaf.exponent, leaf.modulus), "jws sig invalid");

        bytes memory payload = _decodePayload(signingInput);
        return GoogleAttestationPayloadParser.parseRequiredClaims(payload);
    }

    function _verifyCertificate(bytes memory certificateDer, bool expectedCa, VerifiedCert memory parent)
        internal
        view
        returns (VerifiedCert memory cert)
    {
        require(parent.ca, "parent not ca");
        require(parent.notAfter >= block.timestamp, "parent expired");

        cert = _parseCertificate(certificateDer, expectedCa);
        require(cert.issuerHash == parent.subjectHash, "issuer mismatch");

        bytes memory signature = _certificateSignature(certificateDer);
        require(
            RSA.pkcs1Sha256(sha256(_tbsCertificateBytes(certificateDer)), signature, parent.exponent, parent.modulus),
            "cert sig invalid"
        );
    }

    // Certificate ::= SEQUENCE {
    //   tbsCertificate       TBSCertificate,
    //   signatureAlgorithm   AlgorithmIdentifier,
    //   signatureValue       BIT STRING
    // }
    function _parseCertificate(bytes memory certificateDer, bool expectedCa)
        internal
        view
        returns (VerifiedCert memory cert)
    {
        Asn1Ptr certificatePtr = certificateDer.root();
        Asn1Ptr tbsPtr = certificateDer.firstChildOf(certificatePtr);
        Asn1Ptr signatureAlgorithmPtr = certificateDer.nextSiblingOf(tbsPtr);
        require(
            certificateDer.keccak(signatureAlgorithmPtr.contentOffset(), signatureAlgorithmPtr.contentLength())
                == SHA256_WITH_RSA_ENCRYPTION_ALGO_HASH,
            "outer sig algo invalid"
        );

        cert = _parseTbsCertificate(certificateDer, tbsPtr, expectedCa);
    }

    // TBSCertificate ::= SEQUENCE {
    //   version            [0] EXPLICIT Version,
    //   serialNumber            CertificateSerialNumber,
    //   signature               AlgorithmIdentifier,
    //   issuer                  Name,
    //   validity                Validity,
    //   subject                 Name,
    //   subjectPublicKeyInfo    SubjectPublicKeyInfo,
    //   issuerUniqueID     [1]  IMPLICIT OPTIONAL,
    //   subjectUniqueID    [2]  IMPLICIT OPTIONAL,
    //   extensions         [3]  EXPLICIT Extensions
    // }
    //
    // The verifier is intentionally narrow: it assumes Google's certs are conventional v3 RSA
    // certificates and keeps only the fields needed for chain validation.
    function _parseTbsCertificate(bytes memory certificateDer, Asn1Ptr tbsPtr, bool expectedCa)
        internal
        view
        returns (VerifiedCert memory cert)
    {
        Asn1Ptr versionPtr = certificateDer.firstChildOf(tbsPtr);
        Asn1Ptr versionValuePtr = certificateDer.firstChildOf(versionPtr);
        require(certificateDer.uintAt(versionValuePtr) == 2, "version not v3");

        Asn1Ptr serialPtr = certificateDer.nextSiblingOf(versionPtr);
        Asn1Ptr signatureAlgorithmPtr = certificateDer.nextSiblingOf(serialPtr);
        require(
            certificateDer.keccak(signatureAlgorithmPtr.contentOffset(), signatureAlgorithmPtr.contentLength())
                == SHA256_WITH_RSA_ENCRYPTION_ALGO_HASH,
            "tbs sig algo invalid"
        );

        Asn1Ptr issuerPtr = certificateDer.nextSiblingOf(signatureAlgorithmPtr);
        cert.issuerHash = certificateDer.keccak(issuerPtr.contentOffset(), issuerPtr.contentLength());

        Asn1Ptr validityPtr = certificateDer.nextSiblingOf(issuerPtr);
        Asn1Ptr subjectPtr = certificateDer.nextSiblingOf(validityPtr);
        cert.subjectHash = certificateDer.keccak(subjectPtr.contentOffset(), subjectPtr.contentLength());

        Asn1Ptr subjectPublicKeyInfoPtr = certificateDer.nextSiblingOf(subjectPtr);
        Asn1Ptr extensionsPtr = certificateDer.nextSiblingOf(subjectPublicKeyInfoPtr);

        // issuerUniqueID [1] and subjectUniqueID [2] are optional. We do not inspect them,
        // but we skip over them if present so we can still land on the required [3] extensions field.
        if (certificateDer[extensionsPtr.headerOffset()] == 0x81) {
            extensionsPtr = certificateDer.nextSiblingOf(extensionsPtr);
        }
        if (certificateDer[extensionsPtr.headerOffset()] == 0x82) {
            extensionsPtr = certificateDer.nextSiblingOf(extensionsPtr);
        }

        cert.notAfter = _verifyValidity(certificateDer, validityPtr);
        cert.ca = _verifyExtensions(certificateDer, extensionsPtr, expectedCa);
        (cert.exponent, cert.modulus) = _parseRsaPublicKey(certificateDer, subjectPublicKeyInfoPtr);
    }

    function _verifyValidity(bytes memory certificateDer, Asn1Ptr validityPtr) internal view returns (uint64 notAfter) {
        Asn1Ptr notBeforePtr = certificateDer.firstChildOf(validityPtr);
        Asn1Ptr notAfterPtr = certificateDer.nextSiblingOf(notBeforePtr);

        uint256 notBefore = certificateDer.timestampAt(notBeforePtr);
        notAfter = uint64(certificateDer.timestampAt(notAfterPtr));

        require(notBefore <= block.timestamp, "cert not valid yet");
        require(notAfter >= block.timestamp, "cert expired");
    }

    // We only care about the two extensions that affect chain validation for this verifier:
    // - basicConstraints: whether the cert is allowed to act as a CA.
    // - keyUsage: whether the key can sign certificates or signatures.
    function _verifyExtensions(bytes memory certificateDer, Asn1Ptr extensionsPtr, bool expectedCa)
        internal
        pure
        returns (bool isCa)
    {
        require(certificateDer[extensionsPtr.headerOffset()] == 0xa3, "extensions missing");
        extensionsPtr = certificateDer.firstChildOf(extensionsPtr);
        Asn1Ptr extensionPtr = certificateDer.firstChildOf(extensionsPtr);
        uint256 end = extensionsPtr.contentOffset() + extensionsPtr.contentLength();
        bool basicConstraintsFound;
        bool keyUsageFound;

        while (true) {
            Asn1Ptr oidPtr = certificateDer.firstChildOf(extensionPtr);
            bytes32 oid = certificateDer.keccak(oidPtr.contentOffset(), oidPtr.contentLength());

            if (oid == BASIC_CONSTRAINTS_OID_HASH || oid == KEY_USAGE_OID_HASH) {
                Asn1Ptr valuePtr = certificateDer.nextSiblingOf(oidPtr);
                if (certificateDer[valuePtr.headerOffset()] == 0x01) {
                    require(valuePtr.contentLength() == 1, "critical bool invalid");
                    valuePtr = certificateDer.nextSiblingOf(valuePtr);
                }
                valuePtr = certificateDer.octetString(valuePtr);

                if (oid == BASIC_CONSTRAINTS_OID_HASH) {
                    basicConstraintsFound = true;
                    isCa = _verifyBasicConstraintsExtension(certificateDer, valuePtr, expectedCa);
                } else {
                    keyUsageFound = true;
                    _verifyKeyUsageExtension(certificateDer, valuePtr, expectedCa);
                }
            }

            if (extensionPtr.contentOffset() + extensionPtr.contentLength() == end) {
                break;
            }
            extensionPtr = certificateDer.nextSiblingOf(extensionPtr);
        }

        require(basicConstraintsFound, "basic constraints missing");
        require(keyUsageFound, "key usage missing");
    }

    function _verifyBasicConstraintsExtension(bytes memory certificateDer, Asn1Ptr valuePtr, bool expectedCa)
        internal
        pure
        returns (bool isCa)
    {
        if (valuePtr.contentLength() == 0) {
            require(!expectedCa, "basic constraints mismatch");
            return false;
        }

        Asn1Ptr basicConstraintsPtr = certificateDer.firstChildOf(valuePtr);
        if (certificateDer[basicConstraintsPtr.headerOffset()] == 0x01) {
            require(basicConstraintsPtr.contentLength() == 1, "isCA invalid");
            isCa = certificateDer[basicConstraintsPtr.contentOffset()] == 0xff;
        }
        require(isCa == expectedCa, "basic constraints mismatch");
    }

    function _verifyKeyUsageExtension(bytes memory certificateDer, Asn1Ptr valuePtr, bool expectedCa) internal pure {
        uint256 value = _bitstringUintAt(certificateDer, valuePtr);
        if (expectedCa) {
            require(value & 0x04 == 0x04, "certsign missing");
        } else {
            require(value & 0x80 == 0x80, "digital signature missing");
        }
    }

    function _parseRsaPublicKey(bytes memory certificateDer, Asn1Ptr subjectPublicKeyInfoPtr)
        internal
        pure
        returns (bytes memory exponent, bytes memory modulus)
    {
        // SubjectPublicKeyInfo ::= SEQUENCE {
        //   algorithm         AlgorithmIdentifier,
        //   subjectPublicKey  BIT STRING
        // }
        //
        // For RSA keys the BIT STRING wraps an RSAPublicKey sequence:
        // RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }
        Asn1Ptr algorithmPtr = certificateDer.firstChildOf(subjectPublicKeyInfoPtr);
        require(
            certificateDer.keccak(algorithmPtr.contentOffset(), algorithmPtr.contentLength()) == RSA_ENCRYPTION_ALGO_HASH,
            "pubkey algo invalid"
        );

        Asn1Ptr subjectPublicKeyBitStringPtr = certificateDer.nextSiblingOf(algorithmPtr);
        Asn1Ptr rsaPublicKeyPtr = certificateDer.rootOf(certificateDer.bitstring(subjectPublicKeyBitStringPtr));
        Asn1Ptr modulusPtr = certificateDer.firstChildOf(rsaPublicKeyPtr);
        Asn1Ptr exponentPtr = certificateDer.nextSiblingOf(modulusPtr);

        modulus = _positiveIntegerBytes(certificateDer, modulusPtr);
        exponent = _positiveIntegerBytes(certificateDer, exponentPtr);
    }

    function _positiveIntegerBytes(bytes memory certificateDer, Asn1Ptr ptr) internal pure returns (bytes memory) {
        require(certificateDer[ptr.headerOffset()] == 0x02, "not integer");
        uint256 start = ptr.contentOffset();
        uint256 length = ptr.contentLength();
        require(length != 0, "empty integer");
        if (certificateDer[start] == 0x00) {
            start++;
            length--;
        }
        return certificateDer.slice(start, length);
    }

    function _certificateSignature(bytes memory certificateDer) internal pure returns (bytes memory) {
        Asn1Ptr certificatePtr = certificateDer.root();
        Asn1Ptr tbsPtr = certificateDer.firstChildOf(certificatePtr);
        Asn1Ptr signatureAlgorithmPtr = certificateDer.nextSiblingOf(tbsPtr);
        Asn1Ptr signaturePtr = certificateDer.nextSiblingOf(signatureAlgorithmPtr);
        Asn1Ptr signatureBitStringPtr = certificateDer.bitstring(signaturePtr);
        return certificateDer.slice(signatureBitStringPtr.contentOffset(), signatureBitStringPtr.contentLength());
    }

    function _tbsCertificateBytes(bytes memory certificateDer) internal pure returns (bytes memory) {
        Asn1Ptr certificatePtr = certificateDer.root();
        Asn1Ptr tbsPtr = certificateDer.firstChildOf(certificatePtr);
        return certificateDer.slice(tbsPtr.headerOffset(), tbsPtr.encodedLength());
    }

    function _decodePayload(bytes memory signingInput) internal pure returns (bytes memory) {
        // The proof carries detached JWS data as `base64url(header).base64url(payload)` plus a
        // separate signature field, so the signing input must contain exactly one separator.
        uint256 separatorIndex = type(uint256).max;
        for (uint256 i = 0; i < signingInput.length; i++) {
            if (signingInput[i] == ".") {
                require(separatorIndex == type(uint256).max, "multiple separators");
                separatorIndex = i;
            }
        }

        require(separatorIndex != type(uint256).max, "separator missing");
        require(separatorIndex + 1 < signingInput.length, "payload missing");

        bytes memory payloadBase64Url = signingInput.slice(separatorIndex + 1, signingInput.length - separatorIndex - 1);
        return Base64Url.decode(payloadBase64Url);
    }

    function _pinnedRootCert() internal view returns (VerifiedCert memory cert) {
        cert.ca = true;
        cert.notAfter = rootNotAfter;
        cert.subjectHash = rootSubjectHash;
        cert.issuerHash = rootSubjectHash;
        cert.exponent = rootExponent;
        cert.modulus = rootModulus;
    }

    function _bitstringUintAt(bytes memory der, Asn1Ptr ptr) private pure returns (uint256) {
        require(der[ptr.headerOffset()] == 0x03, "not bit string");
        uint256 contentLength = ptr.contentLength() - 1;
        return uint256(_readBytesN(der, ptr.contentOffset() + 1, contentLength) >> ((32 - contentLength) * 8));
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
}
