#!/usr/bin/env python3
"""Update Tempo's embedded ZonePortal runtime when its executable changes."""

import argparse
import hashlib
import re
from pathlib import Path

PORTAL = re.compile(
    r"(pub const ZONE_PORTAL_RUNTIME: Bytes = bytes!\(\n)"
    r"((?:\s*\"[0-9a-fA-Fx]+\"\n)+)(\);)"
)


def runtime(text: str) -> bytes:
    encoded = "".join(re.findall(r'"([0-9a-fA-Fx]+)"', text)).removeprefix("0x")
    return bytes.fromhex(encoded)


def executable(code: bytes) -> bytes:
    """Remove the CBOR metadata whose length is stored in the final two bytes."""
    return code[: -(int.from_bytes(code[-2:], "big") + 2)]


def sha256(code: bytes) -> str:
    return hashlib.sha256(code).hexdigest()


parser = argparse.ArgumentParser()
parser.add_argument("--runtime-file", type=Path, required=True)
parser.add_argument("--tempo-file", type=Path, required=True)
parser.add_argument("--github-output", type=Path, required=True)
args = parser.parse_args()

compiled = bytes.fromhex(args.runtime_file.read_text().strip().removeprefix("0x"))
tempo_source = args.tempo_file.read_text()
match = PORTAL.search(tempo_source)
if not match:
    raise SystemExit("ZONE_PORTAL_RUNTIME not found in Tempo")

zones_executable = executable(compiled)
tempo_executable = executable(runtime(match.group(2)))
changed = zones_executable != tempo_executable
zones_hash = sha256(zones_executable)
tempo_hash = sha256(tempo_executable)

print(f"Zones executable sha256: {zones_hash}")
print(f"Tempo executable sha256: {tempo_hash}")

if changed:
    encoded = compiled.hex()
    chunks = [encoded[i : i + 160] for i in range(0, len(encoded), 160)]
    body = "".join(f'    "{"0x" if i == 0 else ""}{chunk}"\n' for i, chunk in enumerate(chunks))
    replacement = match.group(1) + body + match.group(3)
    args.tempo_file.write_text(tempo_source[: match.start()] + replacement + tempo_source[match.end() :])

with args.github_output.open("a") as output:
    output.write(f"changed={str(changed).lower()}\n")
    output.write(f"zones_hash={zones_hash}\n")
    output.write(f"tempo_hash={tempo_hash}\n")
