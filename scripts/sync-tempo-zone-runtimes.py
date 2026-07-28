#!/usr/bin/env python3
"""Update Tempo's embedded Zone runtimes when their executable code changes."""

import argparse
import hashlib
from pathlib import Path

CONSTANTS = {
    "ZonePortal": "ZONE_PORTAL_RUNTIME",
    "ZoneMessenger": "ZONE_MESSENGER_RUNTIME",
}


def runtime(body: str) -> bytes:
    encoded = "".join(line.strip().strip('"') for line in body.splitlines())
    return bytes.fromhex(encoded.removeprefix("0x"))


def executable(code: bytes) -> bytes:
    """Remove the CBOR metadata whose length is stored in the final two bytes."""
    return code[: -(int.from_bytes(code[-2:], "big") + 2)]


def replace(source: str, constant: str, compiled: bytes) -> tuple[str, bool]:
    start_marker = f"pub const {constant}: Bytes = bytes!(\n"
    start = source.find(start_marker)
    if start == -1:
        raise SystemExit(f"{constant} not found in Tempo")
    body_start = start + len(start_marker)
    end = source.find(");", body_start)
    if end == -1:
        raise SystemExit(f"{constant} is missing its closing delimiter")

    expected = executable(compiled)
    actual = executable(runtime(source[body_start:end]))
    print(f"{constant}: {hashlib.sha256(actual).hexdigest()} → {hashlib.sha256(expected).hexdigest()}")
    if actual == expected:
        return source, False

    encoded = compiled.hex()
    chunks = [encoded[i : i + 160] for i in range(0, len(encoded), 160)]
    body = "".join(f'    "{"0x" if i == 0 else ""}{chunk}"\n' for i, chunk in enumerate(chunks))
    replacement = start_marker + body + ");"
    return source[:start] + replacement + source[end + 2 :], True


parser = argparse.ArgumentParser()
parser.add_argument("--runtime", action="append", required=True, metavar="CONTRACT=FILE")
parser.add_argument("--tempo-file", type=Path, required=True)
parser.add_argument("--github-output", type=Path, required=True)
args = parser.parse_args()

source = args.tempo_file.read_text()
changed = False
for value in args.runtime:
    contract, path = value.split("=", 1)
    source, runtime_changed = replace(
        source, CONSTANTS[contract], bytes.fromhex(Path(path).read_text().strip().removeprefix("0x"))
    )
    changed |= runtime_changed

if changed:
    args.tempo_file.write_text(source)
with args.github_output.open("a") as output:
    output.write(f"changed={str(changed).lower()}\n")
