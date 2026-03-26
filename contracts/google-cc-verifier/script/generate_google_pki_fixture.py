#!/usr/bin/env python3

from __future__ import annotations

import base64
import json
from datetime import datetime, timezone
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from cryptography.x509.oid import NameOID


REPO_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_PATH = REPO_ROOT / "test" / "TestGooglePkiFixture.sol"

ISSUER = "https://confidentialcomputing.googleapis.com"
IMAGE_DIGEST = "sha256:1111111111111111111111111111111111111111111111111111111111111111"
EAT_NONCE = "0x1111111111111111111111111111111111111111111111111111111111111111"
WARP_TIMESTAMP = 1_763_000_000


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _hex_bytes(value: bytes) -> str:
    return value.hex()


def _name(common_name: str) -> x509.Name:
    return x509.Name(
        [
            x509.NameAttribute(NameOID.COUNTRY_NAME, "US"),
            x509.NameAttribute(NameOID.ORGANIZATION_NAME, "Test Google CC"),
            x509.NameAttribute(NameOID.COMMON_NAME, common_name),
        ]
    )


def _certificate(
    *,
    subject: x509.Name,
    issuer: x509.Name,
    public_key,
    issuer_key,
    serial_number: int,
    is_ca: bool,
    path_length: int | None,
):
    not_before = datetime(2025, 1, 1, tzinfo=timezone.utc)
    not_after = datetime(2027, 1, 1, tzinfo=timezone.utc)

    builder = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(issuer)
        .public_key(public_key)
        .serial_number(serial_number)
        .not_valid_before(not_before)
        .not_valid_after(not_after)
        .add_extension(x509.BasicConstraints(ca=is_ca, path_length=path_length), critical=True)
        .add_extension(
            x509.KeyUsage(
                digital_signature=not is_ca,
                content_commitment=False,
                key_encipherment=False,
                data_encipherment=False,
                key_agreement=False,
                key_cert_sign=is_ca,
                crl_sign=is_ca,
                encipher_only=False,
                decipher_only=False,
            ),
            critical=True,
        )
    )
    return builder.sign(private_key=issuer_key, algorithm=hashes.SHA256())


def main() -> None:
    root_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    intermediate_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    leaf_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)

    root_cert = _certificate(
        subject=_name("Test Google CC Root CA"),
        issuer=_name("Test Google CC Root CA"),
        public_key=root_key.public_key(),
        issuer_key=root_key,
        serial_number=1,
        is_ca=True,
        path_length=1,
    )
    intermediate_cert = _certificate(
        subject=_name("Test Google CC Intermediate CA"),
        issuer=root_cert.subject,
        public_key=intermediate_key.public_key(),
        issuer_key=root_key,
        serial_number=2,
        is_ca=True,
        path_length=0,
    )
    leaf_cert = _certificate(
        subject=_name("Test Google CC Leaf"),
        issuer=intermediate_cert.subject,
        public_key=leaf_key.public_key(),
        issuer_key=intermediate_key,
        serial_number=3,
        is_ca=False,
        path_length=None,
    )

    payload = {
        "iss": ISSUER,
        "eat_nonce": EAT_NONCE,
        "iat": WARP_TIMESTAMP,
        "nbf": WARP_TIMESTAMP - 10,
        "exp": WARP_TIMESTAMP + 300,
        "dbgstat": "disabled-since-boot",
        "hwmodel": "GCP_INTEL_TDX",
        "submods": {"container": {"image_digest": IMAGE_DIGEST}},
    }

    header_json = json.dumps({"alg": "RS256", "typ": "JWT"}, separators=(",", ":")).encode("utf-8")
    payload_json = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    signing_input = f"{_b64url(header_json)}.{_b64url(payload_json)}".encode("ascii")
    signature = leaf_key.sign(signing_input, padding.PKCS1v15(), hashes.SHA256())

    fixture = f'''// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

library TestGooglePkiFixture {{
    bytes32 internal constant EAT_NONCE = hex"{EAT_NONCE.removeprefix('0x')}";
    bytes internal constant ROOT_DER = hex"{_hex_bytes(root_cert.public_bytes(serialization.Encoding.DER))}";
    bytes internal constant INTERMEDIATE_DER = hex"{_hex_bytes(intermediate_cert.public_bytes(serialization.Encoding.DER))}";
    bytes internal constant LEAF_DER = hex"{_hex_bytes(leaf_cert.public_bytes(serialization.Encoding.DER))}";
    bytes internal constant SIGNING_INPUT = hex"{_hex_bytes(signing_input)}";
    bytes internal constant JWS_SIGNATURE = hex"{_hex_bytes(signature)}";
    bytes internal constant PAYLOAD_JSON = hex"{_hex_bytes(payload_json)}";
}}
'''

    FIXTURE_PATH.write_text(fixture)


if __name__ == "__main__":
    main()
