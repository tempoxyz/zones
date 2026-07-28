#!/usr/bin/env python3
"""Update Tempo's embedded ZonePortal runtime when its executable changes."""

import argparse
import hashlib
from pathlib import Path

PORTAL_START = "pub const ZONE_PORTAL_RUNTIME: Bytes = bytes!(\n"
PORTAL_END = ");"


def runtime(body: str) -> bytes:
    encoded = "".join(line.strip().strip('"') for line in body.splitlines())
    return bytes.fromhex(encoded.removeprefix("0x"))


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
start = tempo_source.find(PORTAL_START)
if start == -1:
    raise SystemExit("ZONE_PORTAL_RUNTIME not found in Tempo")
body_start = start + len(PORTAL_START)
end = tempo_source.find(PORTAL_END, body_start)
if end == -1:
    raise SystemExit("ZONE_PORTAL_RUNTIME is missing its closing delimiter")

zones_executable = executable(compiled)
tempo_executable = executable(runtime(tempo_source[body_start:end]))
changed = zones_executable != tempo_executable
zones_hash = sha256(zones_executable)
tempo_hash = sha256(tempo_executable)

print(f"Zones executable sha256: {zones_hash}")
print(f"Tempo executable sha256: {tempo_hash}")

if changed:
    encoded = compiled.hex()
    chunks = [encoded[i : i + 160] for i in range(0, len(encoded), 160)]
    body = "".join(f'    "{"0x" if i == 0 else ""}{chunk}"\n' for i, chunk in enumerate(chunks))
    replacement = PORTAL_START + body + PORTAL_END
    args.tempo_file.write_text(tempo_source[:start] + replacement + tempo_source[end + len(PORTAL_END) :])

with args.github_output.open("a") as output:
    output.write(f"changed={str(changed).lower()}\n")
    output.write(f"zones_hash={zones_hash}\n")
    output.write(f"tempo_hash={tempo_hash}\n")
